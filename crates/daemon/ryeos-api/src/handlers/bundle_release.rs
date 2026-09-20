//! Native bundle producer service endpoints.

use std::{
    collections::BTreeMap,
    future::Future,
    io::Read as _,
    path::{Path, PathBuf},
    pin::Pin,
    sync::Arc,
};

use anyhow::Context as _;
use base64::Engine as _;
use ryeos_app::{
    bundle_publication::producer::{
        BundleReleaseAuthorities, BundleReleaseOperation, CatalogRequestPublicationRequest,
        CoreSeedInspectRequest, GenerationBuildRequest, GenerationCaptureRequest,
        GenerationFinalizeRequest, GenerationQualifyRequest, GenesisSetComposeRequest,
        InputInspectRequest, RequestAuthorizationRequest, RequestTreeSigningRequest,
        SetComposeRequest, StatusRequest, SubmitRequest, SubstrateReleaseAuthorizationRequest,
        SubstrateReleaseFinalizeRequest,
    },
    handler_context::HandlerContext,
    service_registry::ServiceDescriptor,
    state::AppState,
};
use ryeos_executor::executor::ServiceAvailability;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use super::bundle_release_execution::{
    PinnedGraphExecution, accept_dispatch_products, execute_pinned_graph, publish_qualification,
    successful_dispatch_result as dispatch_result,
};

const RELEASE_GRAPH_REF: &str = "graph:ryeos/bundle-release/publish";
const NATIVE_BUILD_GRAPH_REF: &str = "graph:ryeos/bundle-release/native-build";
const SIGNED_CAPTURE_GRAPH_REF: &str = "graph:ryeos/bundle-release/signed-capture";
const NATIVE_QUALIFY_TOOL_REF: &str = "tool:ryeos/bundle-release/native-qualify";
const SUBSTRATE_BUILD_GRAPH_REF: &str = "graph:ryeos/bundle-release/substrate-build";
const SUBSTRATE_QUALIFY_GRAPH_REF: &str = "graph:ryeos/bundle-release/substrate-qualify";
const CATALOG_UPLOAD_SERVICE: &str = "service:bundle-catalog/upload";
const CATALOG_PUBLISH_SERVICE: &str = "service:bundle-catalog/publish";
const CATALOG_BLOB_CHUNK_BYTES: usize = 512 * 1024;
const CATALOG_INLINE_BLOB_BYTES: u64 = 16 * 1024 * 1024;

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct CatalogRemotePublishRequest {
    remote: String,
    catalog_namespace: String,
    candidate_publication_attestation_hash: String,
    expected_catalog_head: Option<String>,
}

#[derive(Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct SubstrateBuildRequest {
    project_path: String,
    source_snapshot_hash: String,
    catalog_namespace: String,
    bundle_publication_policy_section_digest: String,
    trust_epoch: u64,
    core_generation_hash: String,
    core_generation_attestation_hash: String,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct SubstrateQualifyRequest {
    project_path: String,
    source_snapshot_hash: String,
    substrate_build_recipe_signed_config: String,
    substrate_build_recipe_raw_digest: String,
    substrate_product_witness: String,
}

#[derive(Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct CoreSeedCaptureRequest {
    build: ryeos_app::bundle_publication::core_seed::CoreSeedBuildRequest,
    selected_product_witness: String,
    build_recipe_signed_config: String,
    build_recipe_raw_digest: String,
    materialization_result_hash: String,
    signed_tree_manifest_hash: String,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct CoreSeedQualifyRequest {
    build: ryeos_app::bundle_publication::core_seed::CoreSeedBuildRequest,
    signed_product_witness: String,
    build_recipe_signed_config: String,
    capture_recipe_signed_config: String,
    capture_recipe_raw_digest: String,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct AuthorityMeasureRequest {
    calibration_run_attestation_hash: String,
    project_path: String,
    catalog_namespace: String,
    publisher_fingerprint: String,
    authorized_uploaders: Vec<String>,
    trust_epoch: u64,
    qualification_owner_principal: String,
    qualification_attestation_hash: String,
    substrate_qualification_owner_principal: String,
    substrate_qualification_attestation_hash: String,
    core_seed_qualification_owner_principal: String,
    core_seed_qualification_attestation_hash: String,
    substrate_build_witness_hash: String,
    substrate_build_signer_fingerprint: String,
    publisher_executable_path: PathBuf,
}

fn authority_measure_handler(
    params: Value,
    context: HandlerContext,
    state: Arc<AppState>,
) -> Pin<Box<dyn Future<Output = anyhow::Result<Value>> + Send>> {
    Box::pin(async move {
        context
            .require_verified()
            .map_err(|error| anyhow::anyhow!(error))?;
        let request: AuthorityMeasureRequest = crate::handler_error::parse_request(params)?;
        anyhow::ensure!(
            request.publisher_executable_path.is_absolute(),
            "publisher executable path must be absolute"
        );
        let publisher_tool =
            ryeos_app::bundle_publication::standalone_publisher::observe_publisher_tool(
                &request.publisher_executable_path,
            )?;
        anyhow::ensure!(request.trust_epoch > 0, "trust epoch must be nonzero");
        anyhow::ensure!(
            request
                .authorized_uploaders
                .windows(2)
                .all(|pair| pair[0] < pair[1]),
            "authorized uploaders must be sorted and unique"
        );
        anyhow::ensure!(
            state
                .engine
                .node_trust_store
                .get(&request.publisher_fingerprint)
                .is_some(),
            "publisher is not pinned in the node trust store"
        );
        let authority = state.state_store.pinned_state_authority()?;
        let guard = authority.acquire_shared_guard()?;
        let calibration_attestation = ryeos_state::objects::Attestation::from_value(
            &authority
                .cas_store()?
                .get_object(&request.calibration_run_attestation_hash)?
                .context("authority calibration run attestation is absent")?,
        )?;
        calibration_attestation.verify_with_key(state.identity.verifying_key())?;
        let calibration = ryeos_app::bundle_publication::calibration::AuthorityCalibrationEvidence::from_attestation(
            &calibration_attestation,
        )?;
        let project = PathBuf::from(&request.project_path).canonicalize()?;
        ryeos_app::bundle_publication::admitted_build::BundleSourceSnapshotAuthority::verify_project_snapshot(
            &ryeos_app::bundle_publication::admitted_build::CleanGitSourceSnapshotAuthority,
            &project,
            &calibration.source_snapshot_hash,
        )?;
        anyhow::ensure!(
            calibration.recipes == expected_calibration_recipes(&project)?,
            "authority calibration measured another fixed recipe set"
        );
        let installed_substrate =
            ryeos_node::load_verified_substrate_identity(&state.config.app_root)?;
        anyhow::ensure!(
            calibration.node_signer_fingerprint == state.identity.fingerprint(),
            "authority calibration belongs to another node"
        );
        anyhow::ensure!(
            calibration.substrate_image_digest == installed_substrate.image_digest
                && calibration.substrate_protocol == installed_substrate.protocol,
            "authority calibration measured another installed substrate"
        );
        anyhow::ensure!(
            calibration.native.owner_principal == request.qualification_owner_principal
                && calibration.native.qualification_attestation_hash
                    == request.qualification_attestation_hash
                && calibration.core_seed.owner_principal
                    == request.core_seed_qualification_owner_principal
                && calibration.core_seed.qualification_attestation_hash
                    == request.core_seed_qualification_attestation_hash
                && calibration.substrate.owner_principal
                    == request.substrate_qualification_owner_principal
                && calibration.substrate.qualification_attestation_hash
                    == request.substrate_qualification_attestation_hash
                && calibration.substrate_product_witness_hash
                    == request.substrate_build_witness_hash
                && calibration.substrate_product_witness_signer_fingerprint
                    == request.substrate_build_signer_fingerprint,
            "authority measurement coordinates differ from the authenticated calibration run"
        );
        let limits = state
            .node_policy
            .require::<ryeos_app::node_policy::sections::object_closure::NodeObjectClosurePolicy>()?
            .closure_limits()?;
        let measured = ryeos_app::operator_external_content::product_qualification::measure_current_qualification_authority(
            &state,
            &context,
            &authority,
            &guard,
            limits,
            &request.qualification_owner_principal,
            &request.qualification_attestation_hash,
        )?;
        let substrate_measured = ryeos_app::operator_external_content::product_qualification::measure_current_qualification_authority(
            &state,
            &context,
            &authority,
            &guard,
            limits,
            &request.substrate_qualification_owner_principal,
            &request.substrate_qualification_attestation_hash,
        )?;
        let core_seed_measured = ryeos_app::operator_external_content::product_qualification::measure_current_qualification_authority(
            &state,
            &context,
            &authority,
            &guard,
            limits,
            &request.core_seed_qualification_owner_principal,
            &request.core_seed_qualification_attestation_hash,
        )?;
        anyhow::ensure!(
            measured.qualification_policy.policy.verifier_ref
                != substrate_measured.qualification_policy.policy.verifier_ref
                && measured.qualification_policy.policy.verifier_ref
                    != core_seed_measured.qualification_policy.policy.verifier_ref
                && substrate_measured.qualification_policy.policy.verifier_ref
                    != core_seed_measured.qualification_policy.policy.verifier_ref,
            "bundle, Core seed, and substrate qualification must use distinct verifiers"
        );
        anyhow::ensure!(
            substrate_measured.required_qualification_claims == ["substrate_release_checks_v1"],
            "substrate qualification must prove the closed substrate release claim"
        );
        anyhow::ensure!(
            core_seed_measured.required_qualification_claims
                == [ryeos_app::bundle_publication::core_seed::QUALIFICATION_CLAIM],
            "Core seed qualification must prove the closed Core seed claim"
        );
        anyhow::ensure!(
            measured.qualification_signer_public_key
                == core_seed_measured.qualification_signer_public_key
                && measured.qualification_signer_public_key
                    == substrate_measured.qualification_signer_public_key
                && measured.qualification_signer_fingerprint
                    == core_seed_measured.qualification_signer_fingerprint
                && measured.qualification_signer_fingerprint
                    == substrate_measured.qualification_signer_fingerprint,
            "bundle, Core seed, and substrate qualification must use one measured signer"
        );
        anyhow::ensure!(
            request.substrate_build_witness_hash
                == substrate_measured.qualified_product_witness_hash,
            "substrate build witness differs from the independently qualified product"
        );
        let substrate_build_signer = state
            .engine
            .node_trust_store
            .get(&request.substrate_build_signer_fingerprint)
            .context("substrate build signer is not pinned in the node trust store")?;
        anyhow::ensure!(
            lillux::crypto::fingerprint(&substrate_build_signer.verifying_key)
                == request.substrate_build_signer_fingerprint,
            "substrate build signer trust entry has an inconsistent fingerprint"
        );
        let substrate_build_witness = ryeos_state::objects::Attestation::from_value(
            &authority
                .cas_store()?
                .get_object(&request.substrate_build_witness_hash)?
                .context("substrate build witness is absent")?,
        )?;
        let substrate_build_evidence = ryeos_state::external_content::products::ProductCaptureEvidence::verify_attestation_for_owner(
            &substrate_build_witness,
            &substrate_build_signer.verifying_key,
            &substrate_measured.qualified_product_owner_principal,
        )?;
        substrate_build_evidence
            .recipe_purpose
            .require_authority_calibration()?;
        anyhow::ensure!(
            substrate_build_evidence.owner_principal
                == substrate_measured.qualified_product_owner_principal,
            "substrate build witness owner differs from qualification testimony"
        );
        let candidate = serde_json::json!({
            "schema": 1,
            "catalogs": [{
                "namespace": request.catalog_namespace,
                "publisher_fingerprint": request.publisher_fingerprint,
                "authorized_uploaders": request.authorized_uploaders,
                "generation_claim": ryeos_app::bundle_publication::attestation::BUNDLE_GENERATION_RELEASE_CLAIM,
                "set_claim": ryeos_app::bundle_publication::attestation::BUNDLE_SET_RELEASE_CLAIM,
                "catalog_publication_claim": ryeos_app::bundle_publication::attestation::BUNDLE_CATALOG_RELEASE_CLAIM,
                "policy": ryeos_app::bundle_publication::attestation::BUNDLE_PUBLICATION_POLICY,
                "trust_epoch": request.trust_epoch,
                "frozen": false,
                "qualification_signer_public_key": measured.qualification_signer_public_key,
                "qualification_signer_fingerprint": measured.qualification_signer_fingerprint,
                "qualification_policy": measured.qualification_policy,
                "qualification_verifier_effective_definition_digest": measured.qualification_verifier_effective_definition_digest,
                "qualification_verifier_artifact_identity": measured.qualification_verifier_artifact_identity,
                "required_qualification_claims": measured.required_qualification_claims,
                "core_seed_qualification_policy": core_seed_measured.qualification_policy,
                "core_seed_qualification_verifier_effective_definition_digest": core_seed_measured.qualification_verifier_effective_definition_digest,
                "core_seed_qualification_verifier_artifact_identity": core_seed_measured.qualification_verifier_artifact_identity,
                "required_core_seed_qualification_claims": core_seed_measured.required_qualification_claims,
                "substrate_qualification_policy": substrate_measured.qualification_policy,
                "substrate_qualification_verifier_effective_definition_digest": substrate_measured.qualification_verifier_effective_definition_digest,
                "substrate_qualification_verifier_artifact_identity": substrate_measured.qualification_verifier_artifact_identity,
                "required_substrate_qualification_claims": substrate_measured.required_qualification_claims,
                "substrate_build_signer_public_key": substrate_build_signer.verifying_key.to_bytes(),
                "substrate_build_signer_fingerprint": request.substrate_build_signer_fingerprint,
                "publisher_tool_effective_definition_digest": publisher_tool.effective_definition_digest,
                "publisher_tool_artifact_identity_hash": publisher_tool.artifact_identity_hash,
            }]
        });
        let policy: ryeos_app::node_policy::sections::bundle_publication::BundlePublicationPolicy =
            serde_json::from_value(candidate.clone())?;
        policy.validate()?;
        Ok(json!({
            "schema": "ryeos.bundle_publication_authority_measurement.v1",
            "calibration_run_attestation_hash": request.calibration_run_attestation_hash,
            "bundle_publication_policy": candidate,
            "bundle_publication_policy_section_digest": policy.section_digest()?,
            "qualification_attestation_hash": request.qualification_attestation_hash,
            "substrate_qualification_attestation_hash": request.substrate_qualification_attestation_hash,
            "core_seed_qualification_attestation_hash": request.core_seed_qualification_attestation_hash,
            "substrate_build_witness_hash": request.substrate_build_witness_hash,
            "publisher_tool": publisher_tool,
        }))
    })
}

fn expected_calibration_recipes(
    project: &Path,
) -> anyhow::Result<ryeos_app::bundle_publication::calibration::AuthorityCalibrationRecipes> {
    use ryeos_app::bundle_publication::calibration::{
        AuthorityCalibrationRecipes, CalibrationRecipeIdentity,
    };
    let identity = |filename: &str| -> anyhow::Result<CalibrationRecipeIdentity> {
        let (signed, raw_content_digest) = source_signed_recipe(
            project,
            &format!("bundles/bundle-release/.ai/config/bundle-release/{filename}"),
        )?;
        Ok(CalibrationRecipeIdentity {
            signed_config_hash: sha256_bytes(signed.as_bytes()),
            raw_content_digest,
        })
    };
    Ok(AuthorityCalibrationRecipes {
        native_build: identity("calibration-native-build-products.yaml")?,
        native_capture: identity("calibration-native-capture-products.yaml")?,
        native_qualification: qualification_recipe_identity(project, "native-qualification.yaml")?,
        core_seed_build: identity("calibration-core-build-products.yaml")?,
        core_seed_capture: identity("calibration-core-capture-products.yaml")?,
        core_seed_qualification: qualification_recipe_identity(
            project,
            "core-seed-qualification.yaml",
        )?,
        substrate_build: identity("calibration-substrate-build-products.yaml")?,
        substrate_qualification: qualification_recipe_identity(
            project,
            "substrate-qualification.yaml",
        )?,
    })
}

fn authority_calibrate_handler(
    params: Value,
    context: HandlerContext,
    state: Arc<AppState>,
) -> Pin<Box<dyn Future<Output = anyhow::Result<Value>> + Send>> {
    Box::pin(async move {
        ryeos_app::operator_authority::require_local_configured_operator(&state, &context)?;
        let request: ryeos_app::bundle_publication::calibration::AuthorityCalibrationRequest =
            crate::handler_error::parse_request(params)?;
        request.validate()?;
        let result = run_authority_calibration(request, context, Arc::clone(&state)).await?;
        result.evidence.validate()?;
        anyhow::ensure!(
            result.evidence.node_signer_fingerprint == state.identity.fingerprint(),
            "authority calibration runner returned evidence for another node"
        );
        let authority = state.state_store.pinned_state_authority()?;
        let attestation = ryeos_state::objects::Attestation::from_value(
            &authority
                .cas_store()?
                .get_object(&result.calibration_run_attestation_hash)?
                .context("authority calibration runner did not retain its attestation")?,
        )?;
        attestation.verify_with_key(state.identity.verifying_key())?;
        anyhow::ensure!(
            ryeos_app::bundle_publication::calibration::AuthorityCalibrationEvidence::from_attestation(
                &attestation,
            )? == result.evidence,
            "authority calibration runner result differs from its retained attestation"
        );
        Ok(serde_json::to_value(result)?)
    })
}

#[derive(Clone)]
struct CalibrationProduct {
    witness_hash: String,
    manifest_hash: String,
}

struct CalibrationLaneResult {
    lane: ryeos_app::bundle_publication::calibration::AuthorityCalibrationLane,
    build: ryeos_app::bundle_publication::calibration::CalibrationRecipeIdentity,
    capture: Option<ryeos_app::bundle_publication::calibration::CalibrationRecipeIdentity>,
    product: CalibrationProduct,
}

async fn run_authority_calibration(
    request: ryeos_app::bundle_publication::calibration::AuthorityCalibrationRequest,
    context: HandlerContext,
    state: Arc<AppState>,
) -> anyhow::Result<ryeos_app::bundle_publication::calibration::AuthorityCalibrationResult> {
    use ryeos_app::bundle_publication::{
        admitted_build::{
            BundleSourceSnapshotAuthority as _, CalibrationBundleInspectRequest,
            CleanGitSourceSnapshotAuthority, PayloadOwnershipConfigItem,
            inspect_calibration_bundle_input, inspect_calibration_core_input,
        },
        calibration::{
            AUTHORITY_CALIBRATION_SCHEMA, AuthorityCalibrationEvidence, AuthorityCalibrationRecipes,
        },
    };

    let project = PathBuf::from(&request.project_path).canonicalize()?;
    CleanGitSourceSnapshotAuthority
        .verify_project_snapshot(&project, &request.source_snapshot_hash)?;
    let loader = ryeos_runtime::verified_loader::VerifiedLoader::new_with_node_config(
        project.clone(),
        state.engine.node_config_root(),
        vec![project.join("bundles/bundle-release")],
        &state.config.runtime_root().trusted_keys_dir(),
    )?;
    let ownership = loader
        .load_config_strict_signed_with_proof::<PayloadOwnershipConfigItem>(
            "bundle-release/payload-ownership",
        )?
        .context("required signed payload ownership config is absent")?
        .value
        .into_current()?;
    let target = json!({
        "kind": "triple",
        "triple": format!("{}-unknown-linux-gnu", std::env::consts::ARCH),
    });
    let native_input = inspect_calibration_bundle_input(
        &ownership,
        &CleanGitSourceSnapshotAuthority,
        CalibrationBundleInspectRequest {
            project_path: project.display().to_string(),
            bundle_name: "bundle-release".to_owned(),
            source_snapshot_hash: request.source_snapshot_hash.clone(),
            target: json!({"kind":"portable"}),
            build_profile: "release".to_owned(),
        },
    )?;
    let core_input = inspect_calibration_core_input(
        &ownership,
        &CleanGitSourceSnapshotAuthority,
        CalibrationBundleInspectRequest {
            project_path: project.display().to_string(),
            bundle_name: "core".to_owned(),
            source_snapshot_hash: request.source_snapshot_hash.clone(),
            target: target.clone(),
            build_profile: "release".to_owned(),
        },
    )?;

    let native = calibrate_signed_bundle(
        "bundle-release",
        NATIVE_BUILD_GRAPH_REF,
        SIGNED_CAPTURE_GRAPH_REF,
        NATIVE_QUALIFY_TOOL_REF,
        "native_bundle",
        "unsigned_bundle",
        "signed_native_bundle",
        "signed_native_bundle_to_release_qualification",
        "calibration-native-build-products.yaml",
        "native-build-products.yaml",
        "config:bundle-release/native-build-products",
        "calibration-native-capture-products.yaml",
        "signed-capture-products.yaml",
        "config:bundle-release/signed-capture-products",
        native_input,
        &project,
        &request.source_snapshot_hash,
        context.clone(),
        Arc::clone(&state),
    )
    .await?;
    let core = calibrate_signed_bundle(
        "core",
        ryeos_app::bundle_publication::core_seed::BUILD_GRAPH,
        ryeos_app::bundle_publication::core_seed::CAPTURE_GRAPH,
        ryeos_app::bundle_publication::core_seed::QUALIFIER,
        "core_seed",
        "unsigned_core",
        "signed_core_seed",
        "signed_core_seed_to_qualification",
        "calibration-core-build-products.yaml",
        "core-seed-build-products.yaml",
        ryeos_app::bundle_publication::core_seed::BUILD_RECIPE,
        "calibration-core-capture-products.yaml",
        "core-seed-capture-products.yaml",
        ryeos_app::bundle_publication::core_seed::CAPTURE_RECIPE,
        core_input,
        &project,
        &request.source_snapshot_hash,
        context.clone(),
        Arc::clone(&state),
    )
    .await?;
    let installed = ryeos_node::load_verified_substrate_identity(&state.config.app_root)?;
    let substrate = calibrate_substrate(
        &installed,
        &core.product.manifest_hash,
        target,
        &project,
        &request.source_snapshot_hash,
        context,
        Arc::clone(&state),
    )
    .await?;
    let evidence = AuthorityCalibrationEvidence {
        schema: AUTHORITY_CALIBRATION_SCHEMA.to_owned(),
        source_snapshot_hash: request.source_snapshot_hash,
        node_signer_fingerprint: state.identity.fingerprint().to_owned(),
        substrate_image_digest: installed.image_digest,
        substrate_protocol: installed.protocol,
        native: native.lane,
        core_seed: core.lane,
        substrate: substrate.lane,
        substrate_product_witness_hash: substrate.product.witness_hash,
        substrate_product_witness_signer_fingerprint: state.identity.fingerprint().to_owned(),
        recipes: AuthorityCalibrationRecipes {
            native_build: native.build,
            native_capture: native
                .capture
                .context("native calibration omitted capture recipe")?,
            native_qualification: qualification_recipe_identity(
                &project,
                "native-qualification.yaml",
            )?,
            core_seed_build: core.build,
            core_seed_capture: core
                .capture
                .context("Core calibration omitted capture recipe")?,
            core_seed_qualification: qualification_recipe_identity(
                &project,
                "core-seed-qualification.yaml",
            )?,
            substrate_build: substrate.build,
            substrate_qualification: qualification_recipe_identity(
                &project,
                "substrate-qualification.yaml",
            )?,
        },
    };
    evidence.validate()?;
    struct IdentitySigner<'a>(&'a ryeos_app::identity::NodeIdentity);
    impl ryeos_state::signer::Signer for IdentitySigner<'_> {
        fn fingerprint(&self) -> &str {
            self.0.fingerprint()
        }
        fn sign(&self, data: &[u8]) -> Vec<u8> {
            use lillux::crypto::Signer as _;
            self.0.signing_key().sign(data).to_bytes().to_vec()
        }
        fn verifying_key(&self) -> lillux::crypto::VerifyingKey {
            *self.0.verifying_key()
        }
    }
    let attestation = evidence.sign_attestation(&IdentitySigner(state.identity.as_ref()))?;
    let hash = state
        .state_store
        .pinned_state_authority()?
        .cas_store()?
        .store_object(&attestation.to_value())?;
    Ok(
        ryeos_app::bundle_publication::calibration::AuthorityCalibrationResult {
            calibration_run_attestation_hash: hash,
            evidence,
        },
    )
}

#[allow(clippy::too_many_arguments)]
async fn calibrate_signed_bundle(
    bundle_name: &str,
    build_graph: &'static str,
    capture_graph: &'static str,
    qualifier: &'static str,
    build_product_name: &str,
    build_declaration: &str,
    capture_product_name: &str,
    qualification_relationship: &str,
    build_source_filename: &str,
    build_overlay_filename: &'static str,
    build_recipe_ref: &'static str,
    capture_source_filename: &str,
    capture_overlay_filename: &'static str,
    capture_recipe_ref: &'static str,
    release_input: Value,
    project: &Path,
    source_snapshot_hash: &str,
    context: HandlerContext,
    state: Arc<AppState>,
) -> anyhow::Result<CalibrationLaneResult> {
    use ryeos_app::bundle_publication::calibration::{
        AuthorityCalibrationLane, CalibrationRecipeIdentity,
    };
    use ryeos_app::bundle_publication::calibration_core::{
        CalibrationCoreManifestAuthority, CalibrationCoreManifestRequest,
    };
    let (build_recipe, build_digest) = source_signed_recipe(
        project,
        &format!("bundles/bundle-release/.ai/config/bundle-release/{build_source_filename}"),
    )?;
    let build_envelope = run_pinned_release_graph(
        build_graph,
        project.to_path_buf(),
        source_snapshot_hash.to_owned(),
        build_recipe.clone(),
        build_overlay_filename,
        false,
        capture_overlay_filename,
        None,
        build_recipe_ref,
        build_digest.clone(),
        json!({"release_input": release_input}),
        Vec::new(),
        None,
        context.clone(),
        Arc::clone(&state),
    )
    .await?;
    let (_, accepted_build) = accept_dispatch_products(&build_envelope, &state)?;
    anyhow::ensure!(
        accepted_build.producer_ref == build_graph && accepted_build.products.len() == 1,
        "{bundle_name} calibration build returned another producer or product set"
    );
    let build_product = &accepted_build.products[0];
    anyhow::ensure!(
        build_product.product_name == build_product_name,
        "{bundle_name} calibration build omitted its declared product"
    );
    let authority = state.state_store.pinned_state_authority()?;
    let cas = Arc::new(authority.cas_store()?);
    let build_manifest = captured_manifest_hash(
        cas.as_ref(),
        &build_product.witness_hash,
        &accepted_build.owner_principal,
        state.identity.verifying_key(),
    )?;
    let signer = CalibrationCoreManifestAuthority::new(
        Arc::clone(&cas),
        state.identity.as_ref().clone(),
        Arc::new(ryeos_app::bundle_publication::admitted_build::CleanGitSourceSnapshotAuthority),
    );
    let signed = signer.sign_and_capture(CalibrationCoreManifestRequest {
        project_path: project.display().to_string(),
        source_snapshot_hash: source_snapshot_hash.to_owned(),
        bundle_name: bundle_name.to_owned(),
        input_content_manifest_hash: build_manifest,
    })?;
    let signed_manifest = String::from_utf8(
        cas.get_blob(signed.evidence().output_manifest_item_hash())?
            .context("calibration signed manifest blob is absent")?,
    )
    .context("calibration signed manifest is not UTF-8")?;
    let (capture_recipe, capture_digest) = source_signed_recipe(
        project,
        &format!("bundles/bundle-release/.ai/config/bundle-release/{capture_source_filename}"),
    )?;
    let selections = vec![ryeos_state::external_content::products::composition::ProductSelectionInput {
        target: ryeos_state::external_content::products::composition::ProductSelectionTarget::Root {},
        selection: ryeos_state::external_content::products::composition::ProductSelection {
            declaration_id: build_declaration.to_owned(),
            witness_hash: build_product.witness_hash.clone(),
            witness_source: ryeos_state::external_content::products::transfer::ProductWitnessSource::LocalCapture {},
            qualification_hash: None,
        },
    }];
    let capture_envelope = run_pinned_release_graph(
        capture_graph,
        project.to_path_buf(),
        source_snapshot_hash.to_owned(),
        build_recipe.clone(),
        build_overlay_filename,
        false,
        capture_overlay_filename,
        Some(capture_recipe.clone()),
        capture_recipe_ref,
        capture_digest.clone(),
        json!({
            "release_input": release_input,
            "materialization_result_hash": signed.evidence_attestation_hash(),
            "signed_tree_manifest_hash": signed.evidence().output_content_manifest_hash(),
            "manifest_item_hash": signed.evidence().output_manifest_item_hash(),
            "signed_manifest": signed_manifest,
        }),
        selections,
        None,
        context.clone(),
        Arc::clone(&state),
    )
    .await?;
    let (_, accepted_capture) = accept_dispatch_products(&capture_envelope, &state)?;
    anyhow::ensure!(
        accepted_capture.producer_ref == capture_graph && accepted_capture.products.len() == 1,
        "{bundle_name} calibration capture returned another producer or product set"
    );
    let capture_product = &accepted_capture.products[0];
    anyhow::ensure!(capture_product.product_name == capture_product_name);
    let capture_manifest = captured_manifest_hash(
        &cas,
        &capture_product.witness_hash,
        &accepted_capture.owner_principal,
        state.identity.verifying_key(),
    )?;
    anyhow::ensure!(
        capture_manifest == signed.evidence().output_content_manifest_hash(),
        "{bundle_name} calibration capture changed the node-signed tree"
    );
    let verifier_chain_root_id = ryeos_app::thread_lifecycle::new_thread_id();
    let qualification_envelope = run_pinned_release_graph(
        qualifier,
        project.to_path_buf(),
        source_snapshot_hash.to_owned(),
        build_recipe.clone(),
        build_overlay_filename,
        false,
        capture_overlay_filename,
        Some(capture_recipe.clone()),
        capture_recipe_ref,
        capture_digest.clone(),
        json!({}),
        vec![ryeos_state::external_content::products::composition::ProductSelectionInput {
            target: ryeos_state::external_content::products::composition::ProductSelectionTarget::Root {},
            selection: ryeos_state::external_content::products::composition::ProductSelection {
                declaration_id: "subject".to_owned(),
                witness_hash: capture_product.witness_hash.clone(),
                witness_source: ryeos_state::external_content::products::transfer::ProductWitnessSource::LocalCapture {},
                qualification_hash: None,
            },
        }],
        Some(verifier_chain_root_id.clone()),
        context.clone(),
        Arc::clone(&state),
    )
    .await?;
    anyhow::ensure!(
        dispatch_result(&qualification_envelope)?["schema"]
            == "ryeos.product_qualification_result.v1",
        "{bundle_name} calibration qualifier returned another contract"
    );
    let qualification = publish_qualification(
        Arc::clone(&state),
        context,
        capture_product.witness_hash.clone(),
        qualification_relationship.to_owned(),
        verifier_chain_root_id.clone(),
        verifier_chain_root_id,
    )
    .await?;
    Ok(CalibrationLaneResult {
        lane: AuthorityCalibrationLane {
            owner_principal: accepted_capture.owner_principal.clone(),
            qualification_attestation_hash: qualification.qualification_hash,
        },
        build: CalibrationRecipeIdentity {
            signed_config_hash: sha256_bytes(build_recipe.as_bytes()),
            raw_content_digest: build_digest,
        },
        capture: Some(CalibrationRecipeIdentity {
            signed_config_hash: sha256_bytes(capture_recipe.as_bytes()),
            raw_content_digest: capture_digest,
        }),
        product: CalibrationProduct {
            witness_hash: capture_product.witness_hash.clone(),
            manifest_hash: capture_manifest,
        },
    })
}

fn captured_manifest_hash(
    cas: &lillux::CasStore,
    witness_hash: &str,
    owner_principal: &str,
    key: &lillux::crypto::VerifyingKey,
) -> anyhow::Result<String> {
    let witness = ryeos_state::objects::Attestation::from_value(
        &cas.get_object(witness_hash)?
            .context("calibration product witness is absent")?,
    )?;
    let evidence = ryeos_state::external_content::products::ProductCaptureEvidence::verify_attestation_for_owner(
        &witness,
        key,
        owner_principal,
    )?;
    evidence.recipe_purpose.require_authority_calibration()?;
    Ok(evidence.manifest_hash)
}

fn sha256_bytes(bytes: &[u8]) -> String {
    use sha2::{Digest as _, Sha256};
    format!("{:x}", Sha256::digest(bytes))
}

fn qualification_recipe_identity(
    project: &Path,
    filename: &str,
) -> anyhow::Result<ryeos_app::bundle_publication::calibration::CalibrationRecipeIdentity> {
    let (signed, raw_content_digest) = source_signed_recipe(
        project,
        &format!("bundles/bundle-release/.ai/config/bundle-release/{filename}"),
    )?;
    Ok(
        ryeos_app::bundle_publication::calibration::CalibrationRecipeIdentity {
            signed_config_hash: sha256_bytes(signed.as_bytes()),
            raw_content_digest,
        },
    )
}

async fn calibrate_substrate(
    installed: &ryeos_node::SubstrateIdentity,
    core_manifest_hash: &str,
    target: Value,
    project: &Path,
    source_snapshot_hash: &str,
    context: HandlerContext,
    state: Arc<AppState>,
) -> anyhow::Result<CalibrationLaneResult> {
    use ryeos_app::bundle_publication::calibration::{
        AuthorityCalibrationLane, CalibrationRecipeIdentity,
    };
    let target: ryeos_bundle_publication_contract::BundleTarget = serde_json::from_value(target)?;
    let receipt = ryeos_bundle_publication_contract::SubstrateBuildReceipt {
        schema: ryeos_bundle_publication_contract::SUBSTRATE_BUILD_RECEIPT_SCHEMA.to_owned(),
        kind: ryeos_bundle_publication_contract::SUBSTRATE_BUILD_RECEIPT_KIND.to_owned(),
        substrate_image_digest: installed.image_digest.clone(),
        substrate_protocol: installed.protocol,
        target,
        // Calibration has no release generation. This field is a content
        // coordinate inside purpose-fenced evidence and binds the installed
        // substrate fixture to the exact node-signed Core calibration tree.
        core_generation_hash: core_manifest_hash.to_owned(),
    };
    receipt.validate()?;
    let (build_recipe, build_digest) = source_signed_recipe(
        project,
        "bundles/bundle-release/.ai/config/bundle-release/calibration-substrate-build-products.yaml",
    )?;
    let build_envelope = run_pinned_release_graph(
        SUBSTRATE_BUILD_GRAPH_REF,
        project.to_path_buf(),
        source_snapshot_hash.to_owned(),
        build_recipe.clone(),
        "substrate-build-products.yaml",
        false,
        "signed-capture-products.yaml",
        None,
        "config:bundle-release/substrate-build-products",
        build_digest.clone(),
        json!({"receipt": receipt}),
        Vec::new(),
        None,
        context.clone(),
        Arc::clone(&state),
    )
    .await?;
    let (_, accepted) = accept_dispatch_products(&build_envelope, &state)?;
    anyhow::ensure!(
        accepted.producer_ref == SUBSTRATE_BUILD_GRAPH_REF && accepted.products.len() == 1,
        "substrate calibration returned another producer or product set"
    );
    let product = &accepted.products[0];
    anyhow::ensure!(product.product_name == "substrate_release");
    let cas = state.state_store.pinned_state_authority()?.cas_store()?;
    let manifest_hash = captured_manifest_hash(
        &cas,
        &product.witness_hash,
        &accepted.owner_principal,
        state.identity.verifying_key(),
    )?;
    let verifier_chain_root_id = ryeos_app::thread_lifecycle::new_thread_id();
    let qualification_envelope = run_pinned_release_graph(
        SUBSTRATE_QUALIFY_GRAPH_REF,
        project.to_path_buf(),
        source_snapshot_hash.to_owned(),
        build_recipe.clone(),
        "substrate-build-products.yaml",
        false,
        "signed-capture-products.yaml",
        None,
        "config:bundle-release/substrate-build-products",
        build_digest.clone(),
        json!({}),
        vec![ryeos_state::external_content::products::composition::ProductSelectionInput {
            target: ryeos_state::external_content::products::composition::ProductSelectionTarget::Root {},
            selection: ryeos_state::external_content::products::composition::ProductSelection {
                declaration_id: "subject".to_owned(),
                witness_hash: product.witness_hash.clone(),
                witness_source: ryeos_state::external_content::products::transfer::ProductWitnessSource::LocalCapture {},
                qualification_hash: None,
            },
        }],
        Some(verifier_chain_root_id.clone()),
        context.clone(),
        Arc::clone(&state),
    )
    .await?;
    anyhow::ensure!(
        dispatch_result(&qualification_envelope)?["schema"]
            == "ryeos.product_qualification_result.v1",
        "substrate calibration qualifier returned another contract"
    );
    let qualification = publish_qualification(
        state,
        context,
        product.witness_hash.clone(),
        "substrate_release_to_qualification".to_owned(),
        verifier_chain_root_id.clone(),
        verifier_chain_root_id,
    )
    .await?;
    Ok(CalibrationLaneResult {
        lane: AuthorityCalibrationLane {
            owner_principal: accepted.owner_principal.clone(),
            qualification_attestation_hash: qualification.qualification_hash,
        },
        build: CalibrationRecipeIdentity {
            signed_config_hash: sha256_bytes(build_recipe.as_bytes()),
            raw_content_digest: build_digest,
        },
        capture: None,
        product: CalibrationProduct {
            witness_hash: product.witness_hash.clone(),
            manifest_hash,
        },
    })
}

fn remote_upload_session(value: &Value) -> anyhow::Result<(&str, Option<&str>)> {
    let session = value
        .get("upload_session_id")
        .and_then(Value::as_str)
        .context("bundle source omitted upload_session_id")?;
    let expected = match value.get("expected_catalog_head") {
        None | Some(Value::Null) => None,
        Some(value) => Some(
            value
                .as_str()
                .context("bundle source returned a malformed expected_catalog_head")?,
        ),
    };
    Ok((session, expected))
}

async fn remote_catalog_call(
    client: &crate::remote::client::RemoteClient,
    service: &str,
    parameters: Value,
) -> anyhow::Result<Value> {
    client
        .execute_service_result(
            service,
            &BTreeMap::new(),
            None,
            &parameters,
            &ryeos_app::execution_policy::ExecutionPolicy::projectless(
                ryeos_app::execution_policy::ExecutionResponse::Wait,
            ),
        )
        .await
}

async fn catalog_remote_publish_handler(
    params: Value,
    context: HandlerContext,
    state: Arc<AppState>,
) -> anyhow::Result<Value> {
    // The CAS guard is deliberately thread-bound. Keep its entire lifetime,
    // including network waits, on one blocking worker instead of moving it
    // between async executor threads or dropping GC protection during upload.
    let runtime = tokio::runtime::Handle::current();
    tokio::task::spawn_blocking(move || {
        runtime.block_on(catalog_remote_publish_locked(params, context, state))
    })
    .await
    .context("catalog remote publication worker failed")?
}

async fn catalog_remote_publish_locked(
    params: Value,
    context: HandlerContext,
    state: Arc<AppState>,
) -> anyhow::Result<Value> {
    context
        .require_verified()
        .map_err(|error| anyhow::anyhow!(error))?;
    let request: CatalogRemotePublishRequest = crate::handler_error::parse_request(params)?;
    let policy = state
        .node_policy
        .require::<ryeos_app::node_policy::sections::bundle_publication::BundlePublicationPolicy>(
    )?;
    let catalog = policy.require_catalog(&request.catalog_namespace)?;
    // The caller already passed service capability admission. The remote call
    // is signed by this node, not by the isolated artifact publisher.
    catalog.require_uploader(state.identity.fingerprint())?;
    // Publication destinations are operator-configured node remotes. Release
    // source content must not be able to redirect authorized-uploader
    // catalog writes through a project-local remotes override.
    let client =
        crate::remote::client::RemoteClient::from_named_remote(&state, &request.remote, None)?;
    let authority = state.state_store.pinned_state_authority()?;
    let _guard = authority.acquire_shared_guard()?;
    let cas = authority.cas_store()?;
    let limits = state
        .node_policy
        .require::<ryeos_app::node_policy::sections::object_closure::NodeObjectClosurePolicy>()?
        .closure_limits()?;
    let closure = ryeos_state::object_closure::collect_object_closure_with_cas_and_limits(
        &cas,
        [request.candidate_publication_attestation_hash.clone()],
        limits,
    )?;
    anyhow::ensure!(
        closure.is_complete(),
        "catalog publication closure is incomplete"
    );
    anyhow::ensure!(
        closure.large_object_hashes.is_empty(),
        "bundle catalog transport does not admit large-object-store references"
    );

    let base = || {
        serde_json::json!({
            "catalog_namespace": request.catalog_namespace.clone(),
            "expected_catalog_head": request.expected_catalog_head.clone(),
            "objects": [],
            "blobs_base64": [],
            "blob_chunks": [],
        })
    };
    let opened = remote_catalog_call(&client, CATALOG_UPLOAD_SERVICE, base()).await?;
    let (session, observed_expected) = remote_upload_session(&opened)?;
    anyhow::ensure!(
        observed_expected == request.expected_catalog_head.as_deref(),
        "bundle source changed the expected catalog head"
    );
    let session = session.to_owned();

    for hash in &closure.blob_hashes {
        let (mut source, total_size) = cas
            .open_blob(hash)?
            .with_context(|| format!("catalog closure blob {hash} disappeared"))?;
        if total_size <= CATALOG_INLINE_BLOB_BYTES {
            let mut bytes = Vec::with_capacity(usize::try_from(total_size)?);
            source.read_to_end(&mut bytes)?;
            anyhow::ensure!(
                u64::try_from(bytes.len())? == total_size,
                "catalog closure blob changed size while reading"
            );
            let mut body = base();
            body["upload_session_id"] = Value::String(session.clone());
            body["blobs_base64"] =
                serde_json::json!([base64::engine::general_purpose::STANDARD.encode(bytes)]);
            let response = remote_catalog_call(&client, CATALOG_UPLOAD_SERVICE, body).await?;
            let (returned, expected) = remote_upload_session(&response)?;
            anyhow::ensure!(
                returned == session && expected == request.expected_catalog_head.as_deref(),
                "bundle source changed the durable upload contract"
            );
            anyhow::ensure!(
                response.get("blob_hashes").and_then(Value::as_array)
                    == Some(&vec![Value::String(hash.clone())]),
                "bundle source stored a blob under another identity"
            );
            continue;
        }
        let mut offset = 0_u64;
        let mut buffer = vec![0_u8; CATALOG_BLOB_CHUNK_BYTES];
        while offset < total_size {
            let read = source.read(&mut buffer)?;
            anyhow::ensure!(
                read != 0,
                "catalog closure blob ended before its declared size"
            );
            let mut body = base();
            body["upload_session_id"] = Value::String(session.clone());
            body["blob_chunks"] = serde_json::json!([{
                "hash": hash,
                "total_size": total_size,
                "offset": offset,
                "data_base64": base64::engine::general_purpose::STANDARD.encode(&buffer[..read]),
            }]);
            let response = remote_catalog_call(&client, CATALOG_UPLOAD_SERVICE, body).await?;
            let (returned, expected) = remote_upload_session(&response)?;
            anyhow::ensure!(
                returned == session && expected == request.expected_catalog_head.as_deref(),
                "bundle source changed the durable upload contract"
            );
            let acknowledged = response
                .get("blob_hashes")
                .and_then(Value::as_array)
                .context("bundle source omitted blob_hashes")?;
            anyhow::ensure!(
                acknowledged.is_empty() || acknowledged.as_slice() == [Value::String(hash.clone())],
                "bundle source acknowledged another blob identity"
            );
            if acknowledged.as_slice() == [Value::String(hash.clone())] {
                break;
            }
            offset = offset
                .checked_add(u64::try_from(read)?)
                .context("blob offset overflow")?;
        }
    }
    for hash in &closure.object_hashes {
        let object = cas
            .get_object(hash)?
            .with_context(|| format!("catalog closure object {hash} disappeared"))?;
        let mut body = base();
        body["upload_session_id"] = Value::String(session.clone());
        body["objects"] = serde_json::json!([object]);
        let response = remote_catalog_call(&client, CATALOG_UPLOAD_SERVICE, body).await?;
        let (returned, expected) = remote_upload_session(&response)?;
        anyhow::ensure!(
            returned == session && expected == request.expected_catalog_head.as_deref(),
            "bundle source changed the durable upload contract"
        );
        anyhow::ensure!(
            response.get("object_hashes").and_then(Value::as_array)
                == Some(&vec![Value::String(hash.clone())]),
            "bundle source stored an object under another identity"
        );
    }

    remote_catalog_call(
        &client,
        CATALOG_PUBLISH_SERVICE,
        serde_json::json!({
            "catalog_namespace": request.catalog_namespace.clone(),
            "candidate_publication_attestation_hash": request.candidate_publication_attestation_hash.clone(),
            "expected_catalog_head": request.expected_catalog_head.clone(),
            "upload_session_id": session,
        }),
    )
    .await
}

struct ContextualReleaseProof<'a> {
    state: &'a AppState,
    authority: ryeos_state::PinnedStateAuthority,
}

impl ryeos_app::bundle_publication::PublisherMaterializationProof for ContextualReleaseProof<'_> {
    fn verify_closed_mutation(
        &self,
        result: &ryeos_bundle_publication_contract::PublisherMaterializationResult,
        input: &ryeos_state::objects::ExternalContentManifestObject,
        output: &ryeos_state::objects::ExternalContentManifestObject,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            result.mutation_contract
                == ryeos_bundle_publication_contract::PublisherMutationContract::RyeosBundleSignV1,
            "publisher used another mutation contract"
        );
        let mut before = input.entries.clone();
        let mut after = output.entries.clone();
        let signed = after
            .iter()
            .find(|entry| entry.path == ".ai/manifest.yaml")
            .context("publisher output omitted .ai/manifest.yaml")?;
        anyhow::ensure!(
            signed.blob_hash.as_deref() == Some(&result.output_manifest_item_hash),
            "publisher manifest-item identity disagrees with output tree"
        );
        before.retain(|entry| entry.path != ".ai/manifest.yaml");
        after.retain(|entry| entry.path != ".ai/manifest.yaml");
        anyhow::ensure!(
            before == after,
            "publisher changed content outside .ai/manifest.yaml"
        );
        Ok(())
    }
}

impl ryeos_app::bundle_publication::BundleReleaseEvidenceProof for ContextualReleaseProof<'_> {
    fn verify_substrate_release_evidence(
        &self,
        release: &ryeos_bundle_publication_contract::SubstrateRelease,
        accepted_result: &ryeos_state::external_content::products::accepted_result::ProductBuildAcceptedResult,
        binding: &ryeos_app::bundle_publication::ReleasePolicyBinding,
    ) -> anyhow::Result<()> {
        let section = self.state.node_policy.require::<
            ryeos_app::node_policy::sections::bundle_publication::BundlePublicationPolicy,
        >()?;
        anyhow::ensure!(
            section.section_digest()? == binding.bundle_publication_policy_section_digest,
            "bundle-publication policy digest is stale"
        );
        let policy = ryeos_app::bundle_publication::standalone_publisher::StandalonePublisherPolicy::from_node_policy(
            section,
            &binding.catalog_namespace,
        )?;
        let proof =
            ryeos_app::bundle_publication::standalone_publisher::StandalonePublisherProof::new(
                Arc::new(self.authority.cas_store()?),
                policy,
            )?;
        ryeos_app::bundle_publication::BundleReleaseEvidenceProof::verify_substrate_release_evidence(
            &proof,
            release,
            accepted_result,
            binding,
        )
    }

    fn verify_release_evidence(
        &self,
        generation: &ryeos_bundle_publication_contract::BundleGeneration,
        accepted: &ryeos_state::external_content::products::accepted_result::ProductBuildAcceptedResult,
        accepted_capture: &ryeos_state::external_content::products::accepted_result::ProductBuildAcceptedResult,
        materialization: &ryeos_bundle_publication_contract::PublisherMaterializationResult,
        binding: &ryeos_app::bundle_publication::ReleasePolicyBinding,
    ) -> anyhow::Result<()> {
        let policy = self.state.node_policy.require::<ryeos_app::node_policy::sections::bundle_publication::BundlePublicationPolicy>()?;
        anyhow::ensure!(
            policy.section_digest()? == binding.bundle_publication_policy_section_digest,
            "bundle-publication policy digest is stale"
        );
        let standalone = ryeos_app::bundle_publication::standalone_publisher::StandalonePublisherPolicy::from_node_policy(
            policy,
            &binding.catalog_namespace,
        )?;
        let proof =
            ryeos_app::bundle_publication::standalone_publisher::StandalonePublisherProof::new(
                Arc::new(self.authority.cas_store()?),
                standalone,
            )?;
        ryeos_app::bundle_publication::BundleReleaseEvidenceProof::verify_release_evidence(
            &proof,
            generation,
            accepted,
            accepted_capture,
            materialization,
            binding,
        )
    }
}

impl ryeos_app::bundle_publication::consumer::ConsumerPublicationPolicy
    for ContextualReleaseProof<'_>
{
    fn verify_publisher_attestation(
        &self,
        attestation: &ryeos_state::objects::Attestation,
        expected_claim: &str,
    ) -> anyhow::Result<()> {
        let fingerprint = attestation.issuer_fingerprint()?;
        let signer = self
            .state
            .engine
            .node_trust_store
            .get(&fingerprint)
            .context("release publisher is not node-trusted")?;
        attestation.verify_with_key(&signer.verifying_key)?;
        anyhow::ensure!(
            attestation.claim == expected_claim
                && attestation.policy
                    == ryeos_app::bundle_publication::attestation::BUNDLE_PUBLICATION_POLICY
                && attestation.expires_at.is_none(),
            "release attestation violates closed policy"
        );
        Ok(())
    }

    fn verify_deployment_attestation(
        &self,
        _attestation: &ryeos_state::objects::Attestation,
    ) -> anyhow::Result<()> {
        anyhow::bail!("deployment attestation is outside release composition authority")
    }
}

async fn dispatch(
    state: Arc<AppState>,
    operation: BundleReleaseOperation,
) -> anyhow::Result<Value> {
    let name = operation.name();
    let authorities = state
        .extensions
        .get::<BundleReleaseAuthorities>()
        .with_context(|| format!(
            "bundle release operation {name} is unavailable: the daemon composition has no explicit build/publisher/qualification/catalog authority adapter"
        ))?;
    authorities.execute(operation).await
}

fn handler<T: DeserializeOwned + Send + 'static>(
    params: Value,
    _context: HandlerContext,
    state: Arc<AppState>,
    wrap: fn(T) -> BundleReleaseOperation,
) -> Pin<Box<dyn Future<Output = anyhow::Result<Value>> + Send>> {
    Box::pin(async move {
        let request = crate::handler_error::parse_request(params)?;
        dispatch(state, wrap(request)).await
    })
}

async fn run_release_graph(
    request: SubmitRequest,
    context: HandlerContext,
    state: Arc<AppState>,
) -> anyhow::Result<Value> {
    async {
        let requested_path = std::path::PathBuf::from(&request.project_path);
        let project_path = requested_path.canonicalize().with_context(|| {
            format!(
                "canonicalize bundle release project {}",
                requested_path.display()
            )
        })?;
        ryeos_app::execution_policy::authorize_standard_local_live_execution(&context.scopes)?;
        let resolved_authority =
            ryeos_app::execution_policy::resolve_standard_local_live_authority(
                &project_path,
                context.scopes.clone(),
                &state.isolation,
            )?;
        let provenance = ryeos_app::execution_provenance::ExecutionProvenance::root_live_fs(
            project_path.clone(),
            Arc::clone(&state.engine),
            resolved_authority.project,
        )?;
        let site_id = state.threads.site_id().to_owned();
        let origin_site_id = context.execution_origin(&site_id);
        let plan_ctx = ryeos_engine::contracts::PlanContext {
            requested_by: ryeos_engine::contracts::EffectivePrincipal::Local(
                ryeos_engine::contracts::Principal {
                    fingerprint: context.fingerprint.clone(),
                    scopes: context.scopes.clone(),
                },
            ),
            project_context: ryeos_engine::contracts::ProjectContext::LocalPath {
                path: project_path.clone(),
            },
            subject_resolution_authority:
                ryeos_engine::contracts::SubjectResolutionAuthority::LiveFs,
            current_site_id: site_id,
            origin_site_id,
            execution_hints: Default::default(),
            scheduled_fire: None,
            validate_only: false,
        };
        let exec_ctx = ryeos_executor::executor::ExecutionContext {
            principal_fingerprint: context.fingerprint.clone(),
            caller_scopes: context.scopes.clone(),
            engine: Arc::clone(&state.engine),
            plan_ctx,
            requested_call: None,
        };
        let parameters = serde_json::to_value(&request)?;
        let acting_principal = context.fingerprint.clone();
        let dispatch_request = ryeos_executor::dispatch::DispatchRequest {
            launch_mode: "wait",
            target_site_id: None,
            validate_only: false,
            params: parameters,
            ref_bindings: Default::default(),
            product_selections: Vec::new(),
            acting_principal: &acting_principal,
            project_path: &project_path,
            provenance,
            lifecycle_authority: resolved_authority.lifecycle,
            launch_timings: None,
            original_root_kind: "graph",
            pre_minted_thread_id: None,
            usage_subject: None,
            usage_subject_asserted_by: None,
            previous_thread_id: None,
            root_admission: None,
            root_dispatch_evidence: None,
            parent_execution_context: None,
            effect_authority: None,
        };
        ryeos_executor::dispatch::dispatch_with_handler_context(
            RELEASE_GRAPH_REF,
            context,
            &dispatch_request,
            &exec_ctx,
            &state,
        )
        .await
        .map_err(|error| anyhow::anyhow!("release Graph dispatch failed: {error}"))
    }
    .await
}

async fn run_pinned_release_graph(
    graph_ref: &'static str,
    source_project: PathBuf,
    expected_source_hash: String,
    signed_build_recipe: String,
    build_recipe_filename: &'static str,
    build_recipe_must_match_source: bool,
    capture_recipe_filename: &'static str,
    signed_capture_recipe: Option<String>,
    recipe_ref: &'static str,
    expected_recipe_raw_digest: String,
    parameters: Value,
    product_selections: Vec<
        ryeos_state::external_content::products::composition::ProductSelectionInput,
    >,
    pre_minted_thread_id: Option<String>,
    context: HandlerContext,
    state: Arc<AppState>,
) -> anyhow::Result<Value> {
    execute_pinned_graph(
        PinnedGraphExecution {
            graph_ref,
            source_project,
            expected_source_hash,
            signed_build_recipe,
            build_recipe_filename,
            build_recipe_must_match_source,
            capture_recipe_filename,
            signed_capture_recipe,
            recipe_ref,
            expected_recipe_raw_digest,
            parameters,
            product_selections,
            pre_minted_thread_id,
        },
        context,
        state,
    )
    .await
}

fn source_signed_recipe(project: &Path, relative: &str) -> anyhow::Result<(String, String)> {
    anyhow::ensure!(
        !Path::new(relative).is_absolute()
            && !Path::new(relative)
                .components()
                .any(|component| matches!(component, std::path::Component::ParentDir)),
        "fixed recipe path is unsafe"
    );
    let output = std::process::Command::new("git")
        .args(["show", &format!("HEAD:{relative}")])
        .current_dir(project)
        .output()
        .context("read fixed recipe from admitted source commit")?;
    anyhow::ensure!(
        output.status.success() && output.stdout.len() <= 256 * 1024,
        "fixed release recipe is absent or oversized in admitted source commit"
    );
    let signed = String::from_utf8(output.stdout).context("fixed release recipe is not UTF-8")?;
    let (body, signature) =
        lillux::signature::strip_canonical_signature_with_envelope(&signed, "#", None, false)?;
    anyhow::ensure!(signature.is_some(), "fixed release recipe is unsigned");
    Ok((signed, lillux::signature::content_hash(&body)))
}

fn substrate_build_handler(
    params: Value,
    context: HandlerContext,
    state: Arc<AppState>,
) -> Pin<Box<dyn Future<Output = anyhow::Result<Value>> + Send>> {
    Box::pin(async move {
        context
            .require_verified()
            .map_err(|error| anyhow::anyhow!(error))?;
        let request: SubstrateBuildRequest = crate::handler_error::parse_request(params)?;
        anyhow::ensure!(request.trust_epoch > 0, "trust epoch must be nonzero");
        let project = PathBuf::from(&request.project_path).canonicalize()?;
        ryeos_app::bundle_publication::admitted_build::BundleSourceSnapshotAuthority::verify_project_snapshot(
            &ryeos_app::bundle_publication::admitted_build::CleanGitSourceSnapshotAuthority,
            &project,
            &request.source_snapshot_hash,
        )?;
        let policy = state.node_policy.require::<
            ryeos_app::node_policy::sections::bundle_publication::BundlePublicationPolicy,
        >()?;
        let catalog = policy.require_catalog(&request.catalog_namespace)?;
        anyhow::ensure!(
            !catalog.frozen
                && catalog.trust_epoch == request.trust_epoch
                && policy.section_digest()? == request.bundle_publication_policy_section_digest,
            "substrate build request does not match current publication policy"
        );
        let receipt = {
            let proof = contextual_proof(&state, &context)?;
            let _guard = proof.authority.acquire_shared_guard()?;
            let cas = proof.authority.cas_store()?;
            let binding = ryeos_app::bundle_publication::ReleasePolicyBinding {
                catalog_namespace: request.catalog_namespace.clone(),
                bundle_publication_policy_section_digest: request
                    .bundle_publication_policy_section_digest
                    .clone(),
                trust_epoch: request.trust_epoch,
            };
            let verified = ryeos_app::bundle_publication::inspect_bundle_generation(
                &request.core_generation_hash,
                &cas,
                &proof,
                &proof,
                &binding,
            )?;
            anyhow::ensure!(
                verified.generation().bundle_name == "core",
                "substrate receipt requires the initial verified Core generation"
            );
            let attestation = ryeos_state::objects::Attestation::from_value(
                &ryeos_app::bundle_publication::PublicationObjectReader::get_object(
                    &cas,
                    &request.core_generation_attestation_hash,
                )?
                .context("Core generation publisher attestation is absent")?,
            )?;
            ryeos_app::bundle_publication::consumer::ConsumerPublicationPolicy::verify_publisher_attestation(
                &proof,
                &attestation,
                ryeos_app::bundle_publication::attestation::BUNDLE_GENERATION_RELEASE_CLAIM,
            )?;
            anyhow::ensure!(
                attestation.subject_hash == request.core_generation_hash,
                "Core publisher attestation subjects another generation"
            );
            let substrate = ryeos_node::load_verified_substrate_identity(&state.config.app_root)?;
            anyhow::ensure!(
                substrate.protocol == verified.generation().substrate_protocol,
                "verified Core generation targets another substrate protocol"
            );
            let receipt = ryeos_bundle_publication_contract::SubstrateBuildReceipt {
                schema: ryeos_bundle_publication_contract::SUBSTRATE_BUILD_RECEIPT_SCHEMA
                    .to_owned(),
                kind: ryeos_bundle_publication_contract::SUBSTRATE_BUILD_RECEIPT_KIND.to_owned(),
                substrate_image_digest: substrate.image_digest,
                substrate_protocol: substrate.protocol,
                target: verified.generation().target.clone(),
                core_generation_hash: request.core_generation_hash.clone(),
            };
            receipt.validate()?;
            receipt
        };
        let (signed_recipe, recipe_digest) = source_signed_recipe(
            &project,
            "bundles/bundle-release/.ai/config/bundle-release/substrate-build-products.yaml",
        )?;
        let result = run_pinned_release_graph(
            SUBSTRATE_BUILD_GRAPH_REF,
            project,
            request.source_snapshot_hash,
            signed_recipe.clone(),
            "substrate-build-products.yaml",
            true,
            "signed-capture-products.yaml",
            None,
            "config:bundle-release/substrate-build-products",
            recipe_digest.clone(),
            json!({"receipt": receipt}),
            Vec::new(),
            None,
            context,
            Arc::clone(&state),
        )
        .await?;
        let (accepted_hash, accepted) = accept_dispatch_products(&result, &state)?;
        anyhow::ensure!(
            accepted.producer_ref == SUBSTRATE_BUILD_GRAPH_REF && accepted.products.len() == 1,
            "substrate build returned another producer or product set"
        );
        let product = &accepted.products[0];
        anyhow::ensure!(
            product.product_name == "substrate_release",
            "substrate build omitted its receipt product"
        );
        let cas = state.state_store.pinned_state_authority()?.cas_store()?;
        let witness = ryeos_state::objects::Attestation::from_value(
            &cas.get_object(&product.witness_hash)?
                .context("substrate product witness is absent")?,
        )?;
        let evidence = ryeos_state::external_content::products::ProductCaptureEvidence::verify_attestation_for_owner(
            &witness,
            state.identity.verifying_key(),
            &accepted.owner_principal,
        )?;
        let manifest = ryeos_state::objects::ExternalContentManifestObject::from_value(
            &cas.get_object(&evidence.manifest_hash)?
                .context("substrate product manifest is absent")?,
        )?;
        let receipt_hash = manifest
            .entries
            .iter()
            .find(|entry| entry.path == ".ai/substrate-release.json")
            .and_then(|entry| entry.blob_hash.clone())
            .context("substrate product omits its receipt blob")?;
        Ok(json!({
            "schema": "ryeos.substrate_release_build_result.v1",
            "accepted_result_hash": accepted_hash,
            "selected_product_identity": product.product_name,
            "selected_product_witness": product.witness_hash,
            "content_manifest_hash": evidence.manifest_hash,
            "substrate_build_receipt_hash": receipt_hash,
            "substrate_image_digest": receipt.substrate_image_digest,
            "substrate_protocol": receipt.substrate_protocol,
            "target": receipt.target,
            "core_generation_hash": receipt.core_generation_hash,
            "substrate_build_recipe_signed_config": signed_recipe,
            "substrate_build_recipe_raw_digest": recipe_digest,
        }))
    })
}

fn substrate_qualify_handler(
    params: Value,
    context: HandlerContext,
    state: Arc<AppState>,
) -> Pin<Box<dyn Future<Output = anyhow::Result<Value>> + Send>> {
    Box::pin(async move {
        let request: SubstrateQualifyRequest = crate::handler_error::parse_request(params)?;
        let selections = vec![ryeos_state::external_content::products::composition::ProductSelectionInput {
            target: ryeos_state::external_content::products::composition::ProductSelectionTarget::Root {},
            selection: ryeos_state::external_content::products::composition::ProductSelection {
                declaration_id: "subject".to_owned(),
                witness_hash: request.substrate_product_witness.clone(),
                witness_source: ryeos_state::external_content::products::transfer::ProductWitnessSource::LocalCapture {},
                qualification_hash: None,
            },
        }];
        let verifier_chain_root_id = ryeos_app::thread_lifecycle::new_thread_id();
        let result = run_pinned_release_graph(
            SUBSTRATE_QUALIFY_GRAPH_REF,
            PathBuf::from(&request.project_path),
            request.source_snapshot_hash,
            request.substrate_build_recipe_signed_config,
            "substrate-build-products.yaml",
            true,
            "signed-capture-products.yaml",
            None,
            "config:bundle-release/substrate-build-products",
            request.substrate_build_recipe_raw_digest,
            json!({}),
            selections,
            Some(verifier_chain_root_id.clone()),
            context.clone(),
            Arc::clone(&state),
        )
        .await?;
        let terminal = dispatch_result(&result)?;
        anyhow::ensure!(
            terminal.get("schema").and_then(Value::as_str)
                == Some("ryeos.product_qualification_result.v1"),
            "substrate qualification returned another contract"
        );
        let publication = publish_qualification(
            Arc::clone(&state),
            context,
            request.substrate_product_witness,
            "substrate_release_to_qualification".to_owned(),
            verifier_chain_root_id.clone(),
            verifier_chain_root_id,
        )
        .await?;
        Ok(json!({
            "schema": "ryeos.substrate_release_qualification_result.v1",
            "qualification_evidence_hashes": [publication.qualification_hash],
        }))
    })
}

fn core_seed_build_handler(
    params: Value,
    context: HandlerContext,
    state: Arc<AppState>,
) -> Pin<Box<dyn Future<Output = anyhow::Result<Value>> + Send>> {
    Box::pin(async move {
        let request: ryeos_app::bundle_publication::core_seed::CoreSeedBuildRequest =
            crate::handler_error::parse_request(params)?;
        request.validate()?;
        let project = request.release_input["project_path"]
            .as_str()
            .context("Core seed release input has no project path")?;
        let source_hash = request.release_input["source_snapshot_hash"]
            .as_str()
            .context("Core seed release input has no source snapshot")?;
        ryeos_app::bundle_publication::admitted_build::BundleSourceSnapshotAuthority::verify_project_snapshot(
            &ryeos_app::bundle_publication::admitted_build::CleanGitSourceSnapshotAuthority,
            Path::new(project), source_hash,
        )?;
        let authored_manifest =
            ryeos_app::bundle_publication::admitted_build::materialize_release_manifest(
                Path::new(project),
                "core",
            )?;
        anyhow::ensure!(
            request.release_input["authored_manifest"] == serde_json::to_value(authored_manifest)?,
            "Core seed manifest differs from the exact source snapshot"
        );
        let policy = state.node_policy.require::<
            ryeos_app::node_policy::sections::bundle_publication::BundlePublicationPolicy,
        >()?;
        let catalog = policy.require_catalog(&request.catalog_namespace)?;
        anyhow::ensure!(
            !catalog.frozen
                && catalog.trust_epoch == request.trust_epoch
                && policy.section_digest()? == request.bundle_publication_policy_section_digest,
            "Core seed build does not match current publication policy"
        );
        let recipe_request =
            ryeos_app::bundle_publication::core_seed::CoreSeedRecipeRequest::Build(request.clone());
        let authorities = state
            .extensions
            .get::<BundleReleaseAuthorities>()
            .context("bundle release publisher authority is unavailable")?;
        let recipe = authorities
            .execute(BundleReleaseOperation::AuthorizeCoreSeedRecipe(
                recipe_request,
            ))
            .await?;
        let signed_recipe = recipe["signed_config"]
            .as_str()
            .context("Core seed recipe authorization omitted signed Config bytes")?
            .to_owned();
        let recipe_digest = recipe["body_hash"]
            .as_str()
            .context("Core seed recipe authorization omitted body identity")?
            .to_owned();
        let result = run_pinned_release_graph(
            ryeos_app::bundle_publication::core_seed::BUILD_GRAPH,
            PathBuf::from(project),
            source_hash.to_owned(),
            signed_recipe.clone(),
            "core-seed-build-products.yaml",
            false,
            "core-seed-capture-products.yaml",
            None,
            ryeos_app::bundle_publication::core_seed::BUILD_RECIPE,
            recipe_digest.clone(),
            json!({"release_input": request.release_input}),
            Vec::new(),
            None,
            context,
            Arc::clone(&state),
        )
        .await?;
        let (accepted_hash, accepted) = accept_dispatch_products(&result, &state)?;
        anyhow::ensure!(
            accepted.producer_ref == ryeos_app::bundle_publication::core_seed::BUILD_GRAPH
                && accepted.products.len() == 1
                && accepted.products[0].product_name == "core_seed",
            "Core seed build returned another producer or product set"
        );
        let product = &accepted.products[0];
        let cas = state.state_store.pinned_state_authority()?.cas_store()?;
        let witness = ryeos_state::objects::Attestation::from_value(
            &cas.get_object(&product.witness_hash)?
                .context("Core seed product witness is absent")?,
        )?;
        let evidence = ryeos_state::external_content::products::ProductCaptureEvidence::verify_attestation_for_owner(
            &witness,
            state.identity.verifying_key(),
            &accepted.owner_principal,
        )?;
        let manifest = ryeos_state::objects::ExternalContentManifestObject::from_value(
            &cas.get_object(&evidence.manifest_hash)?
                .context("Core seed product manifest is absent")?,
        )?;
        ryeos_app::bundle_publication::tree::validate_native_bundle_tree(&manifest)?;
        Ok(json!({
            "schema":"ryeos.core_seed_build_result.v1",
            "accepted_product_result_hash":accepted_hash,
            "selected_product_identity":product.product_name,
            "selected_product_witness":product.witness_hash,
            "input_content_manifest_hash":evidence.manifest_hash,
            "build_recipe_signed_config":signed_recipe,
            "build_recipe_raw_digest":recipe_digest,
        }))
    })
}

fn core_seed_capture_handler(
    params: Value,
    context: HandlerContext,
    state: Arc<AppState>,
) -> Pin<Box<dyn Future<Output = anyhow::Result<Value>> + Send>> {
    Box::pin(async move {
        let request: CoreSeedCaptureRequest = crate::handler_error::parse_request(params)?;
        request.build.validate()?;
        let (_, build_recipe_body) = request
            .build_recipe_signed_config
            .split_once('\n')
            .context("Core seed build recipe signature envelope is absent")?;
        anyhow::ensure!(
            lillux::signature::content_hash(build_recipe_body) == request.build_recipe_raw_digest,
            "Core seed build recipe bytes disagree with their admitted identity"
        );
        let authority = state.state_store.pinned_state_authority()?;
        let cas = authority.cas_store()?;
        let materialization =
            ryeos_bundle_publication_contract::PublisherMaterializationResult::from_current_value(
                &cas.get_object(&request.materialization_result_hash)?
                    .context("Core seed publisher materialization is absent")?,
            )?;
        anyhow::ensure!(
            materialization.output_content_manifest_hash == request.signed_tree_manifest_hash,
            "Core seed capture and materialization disagree on signed tree"
        );
        let signed_manifest = String::from_utf8(
            cas.get_blob(&materialization.output_manifest_item_hash)?
                .context("Core seed signed manifest blob is absent")?,
        )?;
        let recipe_request =
            ryeos_app::bundle_publication::core_seed::CoreSeedRecipeRequest::Capture(
                ryeos_app::bundle_publication::core_seed::CoreSeedCaptureRecipeRequest {
                    build: request.build.clone(),
                    materialization_result_hash: request.materialization_result_hash.clone(),
                    signed_tree_manifest_hash: request.signed_tree_manifest_hash.clone(),
                    manifest_item_hash: materialization.output_manifest_item_hash.clone(),
                    signed_manifest,
                },
            );
        let graph_parameters = recipe_request.parameters();
        let authorities = state
            .extensions
            .get::<BundleReleaseAuthorities>()
            .context("bundle release publisher authority is unavailable")?;
        let recipe = authorities
            .execute(BundleReleaseOperation::AuthorizeCoreSeedRecipe(
                recipe_request,
            ))
            .await?;
        let signed_capture_recipe = recipe["signed_config"]
            .as_str()
            .context("Core seed capture recipe omitted signed Config bytes")?
            .to_owned();
        let capture_recipe_digest = recipe["body_hash"]
            .as_str()
            .context("Core seed capture recipe omitted body identity")?
            .to_owned();
        let selections = vec![ryeos_state::external_content::products::composition::ProductSelectionInput {
            target: ryeos_state::external_content::products::composition::ProductSelectionTarget::Root {},
            selection: ryeos_state::external_content::products::composition::ProductSelection {
                declaration_id: "unsigned_core".to_owned(),
                witness_hash: request.selected_product_witness,
                witness_source: ryeos_state::external_content::products::transfer::ProductWitnessSource::LocalCapture {},
                qualification_hash: None,
            },
        }];
        let project = request.build.release_input["project_path"]
            .as_str()
            .context("Core seed release input has no project path")?;
        let source_hash = request.build.release_input["source_snapshot_hash"]
            .as_str()
            .context("Core seed release input has no source snapshot")?;
        let result = run_pinned_release_graph(
            ryeos_app::bundle_publication::core_seed::CAPTURE_GRAPH,
            PathBuf::from(project),
            source_hash.to_owned(),
            request.build_recipe_signed_config,
            "core-seed-build-products.yaml",
            false,
            "core-seed-capture-products.yaml",
            Some(signed_capture_recipe.clone()),
            ryeos_app::bundle_publication::core_seed::CAPTURE_RECIPE,
            capture_recipe_digest.clone(),
            graph_parameters,
            selections,
            None,
            context,
            Arc::clone(&state),
        )
        .await?;
        let (accepted_hash, accepted) = accept_dispatch_products(&result, &state)?;
        anyhow::ensure!(
            accepted.producer_ref == ryeos_app::bundle_publication::core_seed::CAPTURE_GRAPH
                && accepted.products.len() == 1
                && accepted.products[0].product_name == "signed_core_seed",
            "Core seed capture returned another producer or product set"
        );
        let product = &accepted.products[0];
        Ok(json!({
            "schema":"ryeos.core_seed_capture_result.v1",
            "accepted_capture_result_hash":accepted_hash,
            "signed_product_identity":product.product_name,
            "signed_product_witness":product.witness_hash,
            "capture_recipe_signed_config":signed_capture_recipe,
            "capture_recipe_raw_digest":capture_recipe_digest,
            "materialization_result_hash":request.materialization_result_hash,
            "content_manifest_hash":request.signed_tree_manifest_hash,
            "manifest_item_hash":materialization.output_manifest_item_hash,
            "publisher_fingerprint":materialization.publisher_fingerprint,
        }))
    })
}

fn core_seed_qualify_handler(
    params: Value,
    context: HandlerContext,
    state: Arc<AppState>,
) -> Pin<Box<dyn Future<Output = anyhow::Result<Value>> + Send>> {
    Box::pin(async move {
        let request: CoreSeedQualifyRequest = crate::handler_error::parse_request(params)?;
        request.build.validate()?;
        let selections = vec![ryeos_state::external_content::products::composition::ProductSelectionInput {
            target: ryeos_state::external_content::products::composition::ProductSelectionTarget::Root {},
            selection: ryeos_state::external_content::products::composition::ProductSelection {
                declaration_id: "subject".to_owned(),
                witness_hash: request.signed_product_witness.clone(),
                witness_source: ryeos_state::external_content::products::transfer::ProductWitnessSource::LocalCapture {},
                qualification_hash: None,
            },
        }];
        let verifier_chain_root_id = ryeos_app::thread_lifecycle::new_thread_id();
        let project = request.build.release_input["project_path"]
            .as_str()
            .context("Core seed release input has no project path")?;
        let source_hash = request.build.release_input["source_snapshot_hash"]
            .as_str()
            .context("Core seed release input has no source snapshot")?;
        let result = run_pinned_release_graph(
            ryeos_app::bundle_publication::core_seed::QUALIFIER,
            PathBuf::from(project),
            source_hash.to_owned(),
            request.build_recipe_signed_config,
            "core-seed-build-products.yaml",
            false,
            "core-seed-capture-products.yaml",
            Some(request.capture_recipe_signed_config),
            ryeos_app::bundle_publication::core_seed::CAPTURE_RECIPE,
            request.capture_recipe_raw_digest,
            json!({}),
            selections,
            Some(verifier_chain_root_id.clone()),
            context.clone(),
            Arc::clone(&state),
        )
        .await?;
        anyhow::ensure!(
            dispatch_result(&result)?["schema"] == "ryeos.product_qualification_result.v1",
            "Core seed qualification returned another contract"
        );
        let publication = publish_qualification(
            Arc::clone(&state),
            context,
            request.signed_product_witness,
            "signed_core_seed_to_qualification".to_owned(),
            verifier_chain_root_id.clone(),
            verifier_chain_root_id,
        )
        .await?;
        Ok(json!({
            "schema":"ryeos.core_seed_qualification_result.v1",
            "qualification_evidence_hashes":[publication.qualification_hash],
        }))
    })
}

fn input_inspect_handler(
    params: Value,
    _context: HandlerContext,
    state: Arc<AppState>,
) -> Pin<Box<dyn Future<Output = anyhow::Result<Value>> + Send>> {
    Box::pin(async move {
        let request: InputInspectRequest = crate::handler_error::parse_request(params)?;
        let root = std::path::PathBuf::from(&request.project_path).canonicalize()?;
        let policy = state.node_policy.require::<
            ryeos_app::node_policy::sections::bundle_publication::BundlePublicationPolicy,
        >()?;
        let catalog = policy.require_catalog(&request.catalog_namespace)?;
        anyhow::ensure!(!catalog.frozen, "bundle publication catalog is frozen");
        let ownership_loader =
            ryeos_runtime::verified_loader::VerifiedLoader::new_with_node_config(
                root.clone(),
                state.engine.node_config_root(),
                vec![root.join("bundles/bundle-release")],
                &state.config.runtime_root().trusted_keys_dir(),
            )?;
        let ownership_snapshot = ownership_loader
            .load_config_strict_signed_with_proof::<
                ryeos_app::bundle_publication::admitted_build::PayloadOwnershipConfigItem,
            >("bundle-release/payload-ownership")?
            .context("required signed payload ownership config is absent")?;
        let ownership = ownership_snapshot.value.into_current()?;
        let inspected = ryeos_app::bundle_publication::admitted_build::inspect_release_input(
            &ownership,
            &ryeos_app::bundle_publication::admitted_build::CleanGitSourceSnapshotAuthority,
            request,
        )?;
        let authored_version = inspected["authored_manifest"]["version"]
            .as_str()
            .filter(|version| !version.is_empty())
            .context("bundle manifest has no authored version")?
            .to_owned();
        // Authority and generation metadata belong to the inspection envelope,
        // not the closed build input whose exact bytes are bound into evidence.
        Ok(json!({
            "release_input": inspected,
            "bundle_publication_policy_section_digest": policy.section_digest()?,
            "trust_epoch": catalog.trust_epoch,
            "authored_version": authored_version,
            "bundle_manifest_format": ryeos_bundle::manifest::CURRENT_BUNDLE_MANIFEST_FORMAT,
        }))
    })
}

fn core_seed_inspect_handler(
    params: Value,
    _context: HandlerContext,
    state: Arc<AppState>,
) -> Pin<Box<dyn Future<Output = anyhow::Result<Value>> + Send>> {
    Box::pin(async move {
        let request: CoreSeedInspectRequest = crate::handler_error::parse_request(params)?;
        anyhow::ensure!(
            request.bundle_name == "core",
            "Core seed inspection requires core"
        );
        let root = PathBuf::from(&request.project_path).canonicalize()?;
        let policy = state.node_policy.require::<
            ryeos_app::node_policy::sections::bundle_publication::BundlePublicationPolicy,
        >()?;
        let catalog = policy.require_catalog(&request.catalog_namespace)?;
        let loader = ryeos_runtime::verified_loader::VerifiedLoader::new_with_node_config(
            root.clone(),
            state.engine.node_config_root(),
            vec![root.join("bundles/bundle-release")],
            &state.config.runtime_root().trusted_keys_dir(),
        )?;
        let ownership = loader
            .load_config_strict_signed_with_proof::<
                ryeos_app::bundle_publication::admitted_build::PayloadOwnershipConfigItem,
            >("bundle-release/payload-ownership")?
            .context("required signed payload ownership config is absent")?
            .value
            .into_current()?;
        let inspected = ryeos_app::bundle_publication::admitted_build::inspect_core_seed_input(
            &ownership,
            &ryeos_app::bundle_publication::admitted_build::CleanGitSourceSnapshotAuthority,
            request,
        )?;
        let authored_version = inspected["authored_manifest"]["version"]
            .as_str()
            .context("Core manifest has no authored version")?
            .to_owned();
        let substrate = ryeos_node::load_verified_substrate_identity(&state.config.app_root)?;
        Ok(json!({
            "release_input": inspected,
            "bundle_publication_policy_section_digest": policy.section_digest()?,
            "trust_epoch": catalog.trust_epoch,
            "authored_version": authored_version,
            "bundle_manifest_format": ryeos_bundle::manifest::CURRENT_BUNDLE_MANIFEST_FORMAT,
            "substrate_image_digest": substrate.image_digest,
            "substrate_protocol": substrate.protocol,
        }))
    })
}

fn generation_build_handler(
    params: Value,
    context: HandlerContext,
    state: Arc<AppState>,
) -> Pin<Box<dyn Future<Output = anyhow::Result<Value>> + Send>> {
    Box::pin(async move {
        let request: GenerationBuildRequest = crate::handler_error::parse_request(params)?;
        BundleReleaseOperation::GenerationBuild(request.clone()).validate()?;
        let project = request
            .release_input
            .get("project_path")
            .and_then(Value::as_str)
            .context("release input has no project path")?;
        let source_hash = request
            .release_input
            .get("source_snapshot_hash")
            .and_then(Value::as_str)
            .context("release input has no source snapshot")?;
        ryeos_app::bundle_publication::admitted_build::BundleSourceSnapshotAuthority::verify_project_snapshot(
            &ryeos_app::bundle_publication::admitted_build::CleanGitSourceSnapshotAuthority,
            std::path::Path::new(project), source_hash,
        )?;
        let bundle_name = request.release_input["bundle_name"]
            .as_str()
            .context("release input has no bundle name")?;
        let authored_manifest =
            ryeos_app::bundle_publication::admitted_build::materialize_release_manifest(
                std::path::Path::new(project),
                bundle_name,
            )?;
        anyhow::ensure!(
            request.release_input["authored_manifest"] == serde_json::to_value(authored_manifest)?,
            "release manifest differs from the exact source snapshot"
        );
        let policy = state.node_policy.require::<
            ryeos_app::node_policy::sections::bundle_publication::BundlePublicationPolicy,
        >()?;
        let catalog = policy.require_catalog(&request.catalog_namespace)?;
        anyhow::ensure!(!catalog.frozen, "bundle publication catalog is frozen");
        anyhow::ensure!(
            policy.section_digest()? == request.bundle_publication_policy_section_digest
                && catalog.trust_epoch == request.trust_epoch,
            "build request does not match current bundle publication policy"
        );
        let recipe_request = ryeos_app::bundle_publication::recipe::AuthorizeBuildRecipeRequest {
            catalog_namespace: request.catalog_namespace.clone(),
            bundle_publication_policy_section_digest: request
                .bundle_publication_policy_section_digest
                .clone(),
            trust_epoch: request.trust_epoch,
            release_input: request.release_input.clone(),
        };
        let authorities = state
            .extensions
            .get::<BundleReleaseAuthorities>()
            .context("bundle release publisher authority is unavailable")?;
        let recipe = authorities
            .execute(BundleReleaseOperation::AuthorizeBuildRecipe(recipe_request))
            .await?;
        let signed_recipe = recipe
            .get("signed_config")
            .and_then(Value::as_str)
            .context("publisher recipe authorization omitted signed Config bytes")?
            .to_owned();
        let recipe_raw_digest = recipe
            .get("body_hash")
            .and_then(Value::as_str)
            .context("publisher recipe authorization omitted body identity")?
            .to_owned();
        let result = run_pinned_release_graph(
            NATIVE_BUILD_GRAPH_REF,
            PathBuf::from(project),
            source_hash.to_owned(),
            signed_recipe.clone(),
            "native-build-products.yaml",
            false,
            "signed-capture-products.yaml",
            None,
            ryeos_app::bundle_publication::recipe::BUILD_RECIPE_REF,
            recipe_raw_digest.clone(),
            json!({"release_input": request.release_input}),
            Vec::new(),
            None,
            context.clone(),
            Arc::clone(&state),
        )
        .await?;
        let (accepted_hash, accepted) = accept_dispatch_products(&result, &state)?;
        anyhow::ensure!(
            accepted.producer_ref == NATIVE_BUILD_GRAPH_REF,
            "build result came from another producer"
        );
        let product = accepted
            .products
            .iter()
            .find(|product| product.product_name == "native_bundle")
            .context("build result omitted native_bundle")?;
        anyhow::ensure!(
            accepted.products.len() == 1,
            "build returned undeclared extra products"
        );
        let cas = state.state_store.pinned_state_authority()?.cas_store()?;
        let witness_value = cas
            .get_object(&product.witness_hash)?
            .context("native bundle witness is absent")?;
        let witness = ryeos_state::objects::Attestation::from_value(&witness_value)?;
        let evidence = ryeos_state::external_content::products::ProductCaptureEvidence::verify_attestation_for_owner(
            &witness,
            state.identity.verifying_key(),
            &accepted.owner_principal,
        )?;
        anyhow::ensure!(
            ryeos_state::objects::canonical_value_digest(&witness.to_value())?
                == product.witness_hash,
            "native bundle witness content identity changed"
        );
        let manifest_value = cas
            .get_object(&evidence.manifest_hash)?
            .context("native bundle content manifest is absent")?;
        let manifest =
            ryeos_state::objects::ExternalContentManifestObject::from_value(&manifest_value)?;
        ryeos_app::bundle_publication::tree::validate_native_bundle_tree(&manifest)?;
        Ok(json!({
            "schema":"ryeos.bundle_generation_build_result.v1",
            "accepted_product_result_hash":accepted_hash,
            "selected_product_identity":product.product_name,
            "selected_product_witness":product.witness_hash,
            "input_content_manifest_hash":evidence.manifest_hash,
            "build_execution_evidence_hash":accepted.producer_partition_identity,
            "build_recipe_signed_config":signed_recipe,
            "build_recipe_raw_digest":recipe_raw_digest
        }))
    })
}

fn generation_capture_handler(
    params: Value,
    context: HandlerContext,
    state: Arc<AppState>,
) -> Pin<Box<dyn Future<Output = anyhow::Result<Value>> + Send>> {
    Box::pin(async move {
        let request: GenerationCaptureRequest = crate::handler_error::parse_request(params)?;
        BundleReleaseOperation::GenerationCapture(request.clone()).validate()?;
        let project = request.release_input["project_path"]
            .as_str()
            .context("release input has no project path")?;
        let source_hash = request.release_input["source_snapshot_hash"]
            .as_str()
            .context("release input has no source snapshot")?;
        let policy = state.node_policy.require::<
            ryeos_app::node_policy::sections::bundle_publication::BundlePublicationPolicy,
        >()?;
        let catalog = policy.require_catalog(&request.catalog_namespace)?;
        anyhow::ensure!(
            !catalog.frozen
                && policy.section_digest()? == request.bundle_publication_policy_section_digest
                && catalog.trust_epoch == request.trust_epoch,
            "capture request does not match current bundle publication policy"
        );
        let authority = state.state_store.pinned_state_authority()?;
        let cas = authority.cas_store()?;
        let materialization =
            ryeos_bundle_publication_contract::PublisherMaterializationResult::from_current_value(
                &cas.get_object(&request.materialization_result_hash)?
                    .context("publisher materialization result is absent")?,
            )?;
        anyhow::ensure!(
            materialization.output_content_manifest_hash == request.signed_tree_manifest_hash,
            "capture request and publisher materialization disagree on signed tree"
        );
        let signed_manifest_bytes = cas
            .get_blob(&materialization.output_manifest_item_hash)?
            .context("publisher-signed manifest blob is absent")?;
        let signed_manifest = String::from_utf8(signed_manifest_bytes)
            .context("publisher-signed manifest is not UTF-8")?;
        let (_, build_recipe_body) = request
            .build_recipe_signed_config
            .split_once('\n')
            .context("build recipe signature envelope is absent")?;
        anyhow::ensure!(
            lillux::signature::content_hash(build_recipe_body) == request.build_recipe_raw_digest,
            "retained build recipe bytes disagree with their admitted identity"
        );
        let capture_recipe_request =
            ryeos_app::bundle_publication::recipe::AuthorizeCaptureRecipeRequest {
                catalog_namespace: request.catalog_namespace.clone(),
                bundle_publication_policy_section_digest: request
                    .bundle_publication_policy_section_digest
                    .clone(),
                trust_epoch: request.trust_epoch,
                release_input: request.release_input.clone(),
                materialization_result_hash: request.materialization_result_hash.clone(),
                signed_tree_manifest_hash: request.signed_tree_manifest_hash.clone(),
                manifest_item_hash: materialization.output_manifest_item_hash.clone(),
                signed_manifest,
            };
        let graph_parameters = capture_recipe_request.graph_parameters();
        let authorities = state
            .extensions
            .get::<BundleReleaseAuthorities>()
            .context("bundle release publisher authority is unavailable")?;
        let recipe = authorities
            .execute(BundleReleaseOperation::AuthorizeCaptureRecipe(
                capture_recipe_request,
            ))
            .await?;
        let signed_capture_recipe = recipe["signed_config"]
            .as_str()
            .context("publisher capture recipe omitted signed Config bytes")?
            .to_owned();
        let capture_recipe_digest = recipe["body_hash"]
            .as_str()
            .context("publisher capture recipe omitted body identity")?
            .to_owned();
        let selections = vec![ryeos_state::external_content::products::composition::ProductSelectionInput {
            target: ryeos_state::external_content::products::composition::ProductSelectionTarget::Root {},
            selection: ryeos_state::external_content::products::composition::ProductSelection {
                declaration_id: "unsigned_bundle".to_owned(),
                witness_hash: request.selected_product_witness,
                witness_source: ryeos_state::external_content::products::transfer::ProductWitnessSource::LocalCapture {},
                qualification_hash: None,
            },
        }];
        let result = run_pinned_release_graph(
            SIGNED_CAPTURE_GRAPH_REF,
            PathBuf::from(project),
            source_hash.to_owned(),
            request.build_recipe_signed_config,
            "native-build-products.yaml",
            false,
            "signed-capture-products.yaml",
            Some(signed_capture_recipe.clone()),
            ryeos_app::bundle_publication::recipe::CAPTURE_RECIPE_REF,
            capture_recipe_digest.clone(),
            graph_parameters,
            selections,
            None,
            context,
            Arc::clone(&state),
        )
        .await?;
        let (accepted_hash, accepted) = accept_dispatch_products(&result, &state)?;
        anyhow::ensure!(
            accepted.producer_ref == SIGNED_CAPTURE_GRAPH_REF && accepted.products.len() == 1,
            "signed capture returned another producer or product set"
        );
        let product = &accepted.products[0];
        anyhow::ensure!(
            product.product_name == "signed_native_bundle",
            "signed capture omitted its declared product"
        );
        let witness_value = cas
            .get_object(&product.witness_hash)?
            .context("signed bundle witness is absent")?;
        let witness = ryeos_state::objects::Attestation::from_value(&witness_value)?;
        let evidence = ryeos_state::external_content::products::ProductCaptureEvidence::verify_attestation_for_owner(
            &witness,
            state.identity.verifying_key(),
            &accepted.owner_principal,
        )?;
        anyhow::ensure!(
            evidence.manifest_hash == request.signed_tree_manifest_hash,
            "captured signed-tree witness differs from publisher materialization"
        );
        Ok(json!({
            "schema":"ryeos.bundle_generation_capture.v1",
            "materialization_result_hash":request.materialization_result_hash,
            "content_manifest_hash":evidence.manifest_hash,
            "manifest_item_hash":materialization.output_manifest_item_hash,
            "signed_product_witness":product.witness_hash,
            "signed_product_identity":product.product_name,
            "accepted_capture_result_hash":accepted_hash,
            "capture_recipe_signed_config":signed_capture_recipe,
            "capture_recipe_raw_digest":capture_recipe_digest,
            "publisher_fingerprint":materialization.publisher_fingerprint,
        }))
    })
}

fn generation_qualify_handler(
    params: Value,
    context: HandlerContext,
    state: Arc<AppState>,
) -> Pin<Box<dyn Future<Output = anyhow::Result<Value>> + Send>> {
    Box::pin(async move {
        let request: GenerationQualifyRequest = crate::handler_error::parse_request(params)?;
        BundleReleaseOperation::GenerationQualify(request.clone()).validate()?;
        let project = request
            .release_input
            .get("project_path")
            .and_then(Value::as_str)
            .context("release input has no project path")?;
        let source_hash = request
            .release_input
            .get("source_snapshot_hash")
            .and_then(Value::as_str)
            .context("release input has no source snapshot")?;
        let selections = vec![ryeos_state::external_content::products::composition::ProductSelectionInput {
            target: ryeos_state::external_content::products::composition::ProductSelectionTarget::Root {},
            selection: ryeos_state::external_content::products::composition::ProductSelection {
                declaration_id: "subject".to_owned(),
                witness_hash: request.signed_product_witness.clone(),
                witness_source: ryeos_state::external_content::products::transfer::ProductWitnessSource::LocalCapture {},
                qualification_hash: None,
            },
        }];
        let verifier_chain_root_id = ryeos_app::thread_lifecycle::new_thread_id();
        let result = run_pinned_release_graph(
            NATIVE_QUALIFY_TOOL_REF,
            PathBuf::from(project),
            source_hash.to_owned(),
            request.build_recipe_signed_config.clone(),
            "native-build-products.yaml",
            false,
            "signed-capture-products.yaml",
            Some(request.capture_recipe_signed_config.clone()),
            ryeos_app::bundle_publication::recipe::CAPTURE_RECIPE_REF,
            request.capture_recipe_raw_digest.clone(),
            json!({}),
            selections,
            Some(verifier_chain_root_id.clone()),
            context.clone(),
            Arc::clone(&state),
        )
        .await?;
        let terminal_result = dispatch_result(&result)?;
        anyhow::ensure!(
            terminal_result.get("schema").and_then(Value::as_str)
                == Some("ryeos.product_qualification_result.v1"),
            "qualification returned another contract"
        );
        let publication = publish_qualification(
            Arc::clone(&state),
            context,
            request.signed_product_witness,
            "signed_native_bundle_to_release_qualification".to_owned(),
            verifier_chain_root_id.clone(),
            verifier_chain_root_id,
        )
        .await?;
        Ok(json!({
            "schema":"ryeos.bundle_generation_qualification_result.v1",
            "evidence_hashes":[publication.qualification_hash],
            "provenance_hash":null,
            "sbom_hash":null,
            "qualification_execution_evidence_hash":publication.qualification_hash
        }))
    })
}

fn contextual_proof<'a>(
    state: &'a AppState,
    _context: &'a HandlerContext,
) -> anyhow::Result<ContextualReleaseProof<'a>> {
    let authority = state.state_store.pinned_state_authority()?;
    Ok(ContextualReleaseProof { state, authority })
}

fn generation_finalize_handler(
    params: Value,
    context: HandlerContext,
    state: Arc<AppState>,
) -> Pin<Box<dyn Future<Output = anyhow::Result<Value>> + Send>> {
    Box::pin(async move {
        let request: GenerationFinalizeRequest = crate::handler_error::parse_request(params)?;
        BundleReleaseOperation::GenerationFinalize(request.clone()).validate()?;
        let proof = contextual_proof(&state, &context)?;
        // No await occurs while this transaction owns the thread-bound guard.
        let _guard = proof.authority.acquire_shared_guard()?;
        let generation = ryeos_bundle_publication_contract::BundleGeneration::from_current_value(
            &request.generation,
        )?;
        let cas = proof.authority.cas_store()?;
        let hash = ryeos_app::bundle_publication::finalize_bundle_generation(
            generation,
            &cas,
            &proof,
            &proof,
            &ryeos_app::bundle_publication::ReleasePolicyBinding {
                catalog_namespace: request.catalog_namespace,
                bundle_publication_policy_section_digest: request
                    .bundle_publication_policy_section_digest,
                trust_epoch: request.trust_epoch,
            },
        )?;
        Ok(json!({"schema":"ryeos.bundle_generation_finalize_result.v1","generation_hash":hash}))
    })
}

fn substrate_release_finalize_handler(
    params: Value,
    context: HandlerContext,
    state: Arc<AppState>,
) -> Pin<Box<dyn Future<Output = anyhow::Result<Value>> + Send>> {
    Box::pin(async move {
        let request: SubstrateReleaseFinalizeRequest = crate::handler_error::parse_request(params)?;
        BundleReleaseOperation::SubstrateReleaseFinalize(request.clone()).validate()?;
        let proof = contextual_proof(&state, &context)?;
        let _guard = proof.authority.acquire_shared_guard()?;
        let release = request.release()?;
        let cas = proof.authority.cas_store()?;
        let accepted = ryeos_state::external_content::products::accepted_result::ProductBuildAcceptedResult::from_value(
            &ryeos_app::bundle_publication::PublicationObjectReader::get_object(
                &cas,
                &release.substrate_build_accepted_result_hash,
            )?
            .context("substrate accepted build result is absent")?,
        )?;
        let binding = ryeos_app::bundle_publication::ReleasePolicyBinding {
            catalog_namespace: request.catalog_namespace,
            bundle_publication_policy_section_digest: request
                .bundle_publication_policy_section_digest,
            trust_epoch: request.trust_epoch,
        };
        ryeos_app::bundle_publication::BundleReleaseEvidenceProof::verify_substrate_release_evidence(
            &proof,
            &release,
            &accepted,
            &binding,
        )?;
        let expected = release.content_hash()?;
        let stored = cas.put_object(&release.to_value()?)?;
        anyhow::ensure!(
            stored.hash == expected,
            "stored substrate release identity changed"
        );
        Ok(json!({
            "schema": "ryeos.substrate_release_finalize_result.v1",
            "substrate_release_hash": stored.hash,
        }))
    })
}

fn genesis_set_compose_handler(
    params: Value,
    context: HandlerContext,
    state: Arc<AppState>,
) -> Pin<Box<dyn Future<Output = anyhow::Result<Value>> + Send>> {
    Box::pin(async move {
        let request: GenesisSetComposeRequest = crate::handler_error::parse_request(params)?;
        BundleReleaseOperation::GenesisSetCompose(request.clone()).validate()?;
        let proof = contextual_proof(&state, &context)?;
        let _guard = proof.authority.acquire_shared_guard()?;
        let set =
            ryeos_bundle_publication_contract::BundleSet::from_current_value(&request.bundle_set)?;
        let set_hash = set.content_hash()?;
        let cas = proof.authority.cas_store()?;
        let policy = state.node_policy.require::<
            ryeos_app::node_policy::sections::bundle_publication::BundlePublicationPolicy,
        >()?;
        let expected_publisher = format!(
            "fp:{}",
            policy
                .require_catalog(&request.catalog_namespace)?
                .publisher_fingerprint
        );
        ryeos_app::bundle_publication::consumer::verify_prospective_set(
            set_hash.clone(),
            String::new(),
            set.clone(),
            Some(&expected_publisher),
            &cas,
            &proof,
            &proof,
            &proof,
            &ryeos_app::bundle_publication::ReleasePolicyBinding {
                catalog_namespace: request.catalog_namespace,
                bundle_publication_policy_section_digest: request
                    .bundle_publication_policy_section_digest,
                trust_epoch: request.trust_epoch,
            },
        )?;
        let stored = cas.put_object(&set.to_value()?)?;
        anyhow::ensure!(
            stored.hash == set_hash,
            "stored genesis set identity changed"
        );
        Ok(json!({
            "schema": "ryeos.genesis_set_compose_result.v1",
            "bundle_set_hash": set_hash,
        }))
    })
}

fn set_compose_handler(
    params: Value,
    context: HandlerContext,
    state: Arc<AppState>,
) -> Pin<Box<dyn Future<Output = anyhow::Result<Value>> + Send>> {
    Box::pin(async move {
        let request: SetComposeRequest = crate::handler_error::parse_request(params)?;
        BundleReleaseOperation::SetCompose(request.clone()).validate()?;
        let proof = contextual_proof(&state, &context)?;
        // Retain GC exclusion from the first read through the final CAS write.
        let _guard = proof.authority.acquire_shared_guard()?;
        let cas = proof.authority.cas_store()?;
        let mut set = ryeos_bundle_publication_contract::BundleSet::from_current_value(
            &ryeos_app::bundle_publication::PublicationObjectReader::get_object(
                &cas,
                &request.predecessor_set_hash,
            )?
            .context("predecessor bundle set is absent")?,
        )?;
        anyhow::ensure!(
            set.substrate_protocol == request.substrate_protocol,
            "set substrate protocol changed"
        );
        let replacement: ryeos_bundle_publication_contract::BundleSetEntry =
            serde_json::from_value(request.replacement)?;
        match set
            .entries
            .binary_search_by(|entry| entry.bundle_name.cmp(&replacement.bundle_name))
        {
            Ok(i) => set.entries[i] = replacement,
            Err(i) => set.entries.insert(i, replacement),
        }
        set.validate()?;
        let set_hash = set.content_hash()?;
        let binding = ryeos_app::bundle_publication::ReleasePolicyBinding {
            catalog_namespace: request.catalog_namespace.clone(),
            bundle_publication_policy_section_digest: request
                .bundle_publication_policy_section_digest,
            trust_epoch: request.trust_epoch,
        };
        let policy = state.node_policy.require::<ryeos_app::node_policy::sections::bundle_publication::BundlePublicationPolicy>()?;
        let expected = format!(
            "fp:{}",
            policy
                .require_catalog(&request.catalog_namespace)?
                .publisher_fingerprint
        );
        ryeos_app::bundle_publication::consumer::verify_prospective_set(
            set_hash.clone(),
            String::new(),
            set.clone(),
            Some(&expected),
            &cas,
            &proof,
            &proof,
            &proof,
            &binding,
        )?;
        let stored = cas.put_object(&set.to_value()?)?;
        anyhow::ensure!(
            stored.hash == set_hash,
            "stored bundle set identity changed"
        );
        Ok(
            json!({"schema":"ryeos.bundle_set_compose_result.v1","predecessor_set_hash":request.predecessor_set_hash,"bundle_set_hash":set_hash}),
        )
    })
}

async fn dispatch_release_graph(
    request: SubmitRequest,
    context: HandlerContext,
    state: Arc<AppState>,
) -> anyhow::Result<Value> {
    anyhow::ensure!(
        context.verified,
        "bundle release submission requires an authenticated caller"
    );
    let operation = BundleReleaseOperation::Submit(request.clone());
    operation.validate()?;
    let authorities = state
        .extensions
        .get::<BundleReleaseAuthorities>()
        .context("bundle release lifecycle authority is unavailable")?;
    let operation_id = authorities.begin_external(&operation)?;
    let task_operation_id = operation_id.clone();
    tokio::spawn(async move {
        let outcome = run_release_graph(request, context, state).await;
        match outcome {
            Ok(result) => {
                if let Err(error) = authorities.complete_external(&task_operation_id, &result) {
                    tracing::error!(
                        operation_id = %task_operation_id,
                        %error,
                        "failed to record completed bundle release operation"
                    );
                }
            }
            Err(error) => {
                let rendered = format!("{error:#}");
                if let Err(record_error) = authorities.fail_external(&task_operation_id, &rendered)
                {
                    tracing::error!(
                        operation_id = %task_operation_id,
                        error = %record_error,
                        "failed to record failed bundle release operation"
                    );
                }
            }
        }
    });

    Ok(json!({
        "schema": "ryeos.bundle_release_submission.v1",
        "operation_id": operation_id,
        "state": "submitted"
    }))
}

fn submit_handler(
    params: Value,
    context: HandlerContext,
    state: Arc<AppState>,
) -> Pin<Box<dyn Future<Output = anyhow::Result<Value>> + Send>> {
    Box::pin(async move {
        let request = crate::handler_error::parse_request(params)?;
        dispatch_release_graph(request, context, state).await
    })
}

macro_rules! descriptor {
    ($const_name:ident, $service:literal, $endpoint:literal, $cap:literal, $request:ty, $variant:path) => {
        pub const $const_name: ServiceDescriptor = ServiceDescriptor {
            service_ref: $service,
            endpoint: $endpoint,
            availability: ServiceAvailability::DaemonOnly,
            required_caps: &[$cap],
            handler: |params, context, state| handler::<$request>(params, context, state, $variant),
        };
    };
}

pub const INPUT_INSPECT: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:bundle-release/input-inspect",
    endpoint: "bundle_release.input_inspect",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.bundle-release/input-inspect"],
    handler: input_inspect_handler,
};
pub const AUTHORITY_MEASURE: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:bundle-release/authority-measure",
    endpoint: "bundle_release.authority_measure",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.bundle-release/authority-measure"],
    handler: authority_measure_handler,
};
pub const AUTHORITY_CALIBRATE: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:bundle-release/authority-calibrate",
    endpoint: "bundle_release.authority_calibrate",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &[
        "ryeos.execute.config.bundle-release/core-seed-build-products",
        "ryeos.execute.config.bundle-release/core-seed-capture-products",
        "ryeos.execute.config.bundle-release/native-build-products",
        "ryeos.execute.config.bundle-release/signed-capture-products",
        "ryeos.execute.config.bundle-release/substrate-build-products",
        "ryeos.execute.service.bundle-release/authority-calibrate",
        "ryeos.execute.tool.ryeos/bundle-release/core-seed-build",
        "ryeos.execute.tool.ryeos/bundle-release/core-seed-capture",
        "ryeos.execute.tool.ryeos/bundle-release/core-seed-qualify",
        "ryeos.execute.tool.ryeos/bundle-release/native-build",
        "ryeos.execute.tool.ryeos/bundle-release/native-qualify",
        "ryeos.execute.tool.ryeos/bundle-release/signed-capture",
        "ryeos.execute.tool.ryeos/bundle-release/substrate-build",
        "ryeos.execute.tool.ryeos/bundle-release/substrate-qualify",
    ],
    handler: authority_calibrate_handler,
};
pub const GENERATION_BUILD: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:bundle-release/generation-build",
    endpoint: "bundle_release.generation_build",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &[
        "ryeos.execute.config.bundle-release/native-build-products",
        "ryeos.execute.service.bundle-release/generation-build",
        "ryeos.execute.tool.ryeos/bundle-release/native-build",
    ],
    handler: generation_build_handler,
};
pub const CORE_SEED_BUILD: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:bundle-release/core-seed-build",
    endpoint: "bundle_release.core_seed_build",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &[
        "ryeos.execute.config.bundle-release/core-seed-build-products",
        "ryeos.execute.service.bundle-release/core-seed-build",
        "ryeos.execute.tool.ryeos/bundle-release/core-seed-build",
    ],
    handler: core_seed_build_handler,
};
pub const CORE_SEED_INSPECT: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:bundle-release/core-seed-inspect",
    endpoint: "bundle_release.core_seed_inspect",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.bundle-release/core-seed-inspect"],
    handler: core_seed_inspect_handler,
};
pub const CORE_SEED_CAPTURE: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:bundle-release/core-seed-capture",
    endpoint: "bundle_release.core_seed_capture",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &[
        "ryeos.execute.config.bundle-release/core-seed-capture-products",
        "ryeos.execute.service.bundle-release/core-seed-capture",
        "ryeos.execute.tool.ryeos/bundle-release/core-seed-capture",
    ],
    handler: core_seed_capture_handler,
};
pub const CORE_SEED_QUALIFY: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:bundle-release/core-seed-qualify",
    endpoint: "bundle_release.core_seed_qualify",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &[
        "ryeos.execute.service.bundle-release/core-seed-qualify",
        "ryeos.execute.tool.ryeos/bundle-release/core-seed-qualify",
    ],
    handler: core_seed_qualify_handler,
};
descriptor!(
    REQUEST_TREE_SIGNING,
    "service:bundle-release/request-tree-signing",
    "bundle_release.request_tree_signing",
    "ryeos.execute.service.bundle-release/request-tree-signing",
    RequestTreeSigningRequest,
    BundleReleaseOperation::RequestTreeSigning
);
pub const GENERATION_CAPTURE: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:bundle-release/generation-capture",
    endpoint: "bundle_release.generation_capture",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &[
        "ryeos.execute.config.bundle-release/signed-capture-products",
        "ryeos.execute.service.bundle-release/generation-capture",
        "ryeos.execute.tool.ryeos/bundle-release/signed-capture",
    ],
    handler: generation_capture_handler,
};
pub const GENERATION_QUALIFY: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:bundle-release/generation-qualify",
    endpoint: "bundle_release.generation_qualify",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &[
        "ryeos.execute.service.bundle-release/generation-qualify",
        "ryeos.execute.tool.ryeos/bundle-release/native-qualify",
    ],
    handler: generation_qualify_handler,
};
pub const GENERATION_FINALIZE: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:bundle-release/generation-finalize",
    endpoint: "bundle_release.generation_finalize",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.bundle-release/generation-finalize"],
    handler: generation_finalize_handler,
};
pub const SUBSTRATE_BUILD: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:bundle-release/substrate-build",
    endpoint: "bundle_release.substrate_build",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &[
        "ryeos.execute.config.bundle-release/substrate-build-products",
        "ryeos.execute.service.bundle-release/substrate-build",
        "ryeos.execute.tool.ryeos/bundle-release/substrate-build",
    ],
    handler: substrate_build_handler,
};
pub const SUBSTRATE_QUALIFY: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:bundle-release/substrate-qualify",
    endpoint: "bundle_release.substrate_qualify",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &[
        "ryeos.execute.service.bundle-release/substrate-qualify",
        "ryeos.execute.tool.ryeos/bundle-release/substrate-qualify",
    ],
    handler: substrate_qualify_handler,
};
pub const SUBSTRATE_RELEASE_FINALIZE: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:bundle-release/substrate-release-finalize",
    endpoint: "bundle_release.substrate_release_finalize",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.bundle-release/substrate-release-finalize"],
    handler: substrate_release_finalize_handler,
};
descriptor!(
    SUBSTRATE_RELEASE_AUTHORIZATION,
    "service:bundle-release/substrate-release-authorization",
    "bundle_release.substrate_release_authorization",
    "ryeos.execute.service.bundle-release/substrate-release-authorization",
    SubstrateReleaseAuthorizationRequest,
    BundleReleaseOperation::SubstrateReleaseAuthorization
);
descriptor!(
    REQUEST_AUTHORIZATION,
    "service:bundle-release/request-authorization",
    "bundle_release.request_authorization",
    "ryeos.execute.service.bundle-release/request-authorization",
    RequestAuthorizationRequest,
    BundleReleaseOperation::RequestAuthorization
);
pub const SET_COMPOSE: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:bundle-release/set-compose",
    endpoint: "bundle_release.set_compose",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.bundle-release/set-compose"],
    handler: set_compose_handler,
};
pub const GENESIS_SET_COMPOSE: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:bundle-release/genesis-set-compose",
    endpoint: "bundle_release.genesis_set_compose",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.bundle-release/genesis-set-compose"],
    handler: genesis_set_compose_handler,
};
descriptor!(
    CATALOG_REQUEST_PUBLICATION,
    "service:bundle-release/catalog-request-publication",
    "bundle_release.catalog_request_publication",
    "ryeos.execute.service.bundle-release/catalog-request-publication",
    CatalogRequestPublicationRequest,
    BundleReleaseOperation::CatalogRequestPublication
);
pub const CATALOG_REMOTE_PUBLISH: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:bundle-release/catalog-remote-publish",
    endpoint: "bundle_release.catalog_remote_publish",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.bundle-release/catalog-remote-publish"],
    handler: |params, context, state| {
        Box::pin(catalog_remote_publish_handler(params, context, state))
    },
};
pub const SUBMIT: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:bundle-release/submit",
    endpoint: "bundle_release.submit",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.bundle-release/submit"],
    handler: submit_handler,
};
descriptor!(
    STATUS,
    "service:bundle-release/status",
    "bundle_release.status",
    "ryeos.execute.service.bundle-release/status",
    StatusRequest,
    BundleReleaseOperation::Status
);
pub const ALL: &[ServiceDescriptor] = &[
    AUTHORITY_CALIBRATE,
    AUTHORITY_MEASURE,
    INPUT_INSPECT,
    CORE_SEED_INSPECT,
    CORE_SEED_BUILD,
    CORE_SEED_CAPTURE,
    CORE_SEED_QUALIFY,
    GENERATION_BUILD,
    REQUEST_TREE_SIGNING,
    GENERATION_CAPTURE,
    GENERATION_QUALIFY,
    GENERATION_FINALIZE,
    SUBSTRATE_BUILD,
    SUBSTRATE_QUALIFY,
    SUBSTRATE_RELEASE_FINALIZE,
    SUBSTRATE_RELEASE_AUTHORIZATION,
    REQUEST_AUTHORIZATION,
    SET_COMPOSE,
    GENESIS_SET_COMPOSE,
    CATALOG_REQUEST_PUBLICATION,
    CATALOG_REMOTE_PUBLISH,
    SUBMIT,
    STATUS,
];

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    use crate::handlers::bundle_release_execution::materialize_execution_project;

    #[test]
    fn release_execution_project_is_exact_head_with_only_fixed_recipe_replaced() {
        let source = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(source.path().join(".ai/knowledge")).unwrap();
        std::fs::write(source.path().join("source.txt"), b"source\n").unwrap();
        std::fs::write(source.path().join(".ai/knowledge/existing.md"), b"kept\n").unwrap();
        for args in [
            vec!["init", "-q"],
            vec!["add", "."],
            vec![
                "-c",
                "user.name=RyeOS Test",
                "-c",
                "user.email=ryeos@example.invalid",
                "commit",
                "-qm",
                "fixture",
            ],
        ] {
            assert!(
                Command::new("git")
                    .args(args)
                    .current_dir(source.path())
                    .status()
                    .unwrap()
                    .success()
            );
        }
        let signed = "# ryeos:signed:test\nexact: recipe\n";
        let source_hash = ryeos_app::bundle_publication::admitted_build::CleanGitSourceSnapshotAuthority::snapshot_hash(source.path()).unwrap();
        let workspace = materialize_execution_project(
            source.path(),
            &source_hash,
            signed,
            "native-build-products.yaml",
            false,
            "signed-capture-products.yaml",
            None,
        )
        .unwrap();
        assert_eq!(
            std::fs::read(workspace.path().join("source.txt")).unwrap(),
            b"source\n"
        );
        assert_eq!(
            std::fs::read_to_string(
                workspace
                    .path()
                    .join(".ai/config/bundle-release/native-build-products.yaml")
            )
            .unwrap(),
            signed
        );
        assert!(!workspace.path().join(".git").exists());
        assert!(
            materialize_execution_project(
                source.path(),
                &"f".repeat(64),
                signed,
                "native-build-products.yaml",
                false,
                "signed-capture-products.yaml",
                None,
            )
            .unwrap_err()
            .to_string()
            .contains("HEAD changed")
        );
    }

    #[test]
    fn contextual_proof_does_not_retain_thread_bound_guard() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<ContextualReleaseProof<'static>>();
    }

    #[test]
    fn release_dispatch_result_requires_successful_nested_terminal_value() {
        let value = json!({
            "thread":{"thread_id":"thread:test"},
            "result":{"outcome_code":"success","result":{"schema":"expected"},"error":null,"artifacts":[]}
        });
        assert_eq!(dispatch_result(&value).unwrap()["schema"], "expected");
        let failed = json!({"result":{"outcome_code":"failed","result":null}});
        assert!(dispatch_result(&failed).is_err());
    }
}
