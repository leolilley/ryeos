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
    PinnedGraphExecution, ReleaseSourceGeneration, accept_dispatch_products, execute_pinned_graph,
    publish_qualification, successful_dispatch_result as dispatch_result,
};

const RELEASE_GRAPH_REF: &str = "graph:ryeos/bundle-release/publish";
const NATIVE_QUALIFY_TOOL_REF: &str = "tool:ryeos/bundle-release/native-qualify";
const SUBSTRATE_BUILD_GRAPH_REF: &str = "graph:ryeos/bundle-release/substrate-build";
const SUBSTRATE_QUALIFY_TOOL_REF: &str = "tool:ryeos/bundle-release/substrate-qualify";
const CATALOG_UPLOAD_SERVICE: &str = "service:bundle-catalog/upload";
const CATALOG_PUBLISH_SERVICE: &str = "service:bundle-catalog/publish";
const CATALOG_BLOB_CHUNK_BYTES: usize = 512 * 1024;
const CATALOG_INLINE_BLOB_BYTES: u64 = 16 * 1024 * 1024;

async fn resolve_release_source_generation(
    project_identity: &Path,
    snapshot_hash: &str,
    context: &HandlerContext,
    state: &Arc<AppState>,
) -> anyhow::Result<(
    Arc<ReleaseSourceGeneration>,
    ryeos_executor::execution::project_source::ResolvedProjectContext,
)> {
    // Bind the content coordinate to the authenticated principal's canonical
    // project HEAD. Knowledge of an arbitrary CAS hash is not project access.
    let source = ryeos_executor::execution::project_source::ProjectSource::PushedHead;
    let mut project =
        crate::routes::response_modes::execute_mode::resolve_project_context_off_thread(
            crate::routes::response_modes::execute_mode::ResolveProjectContextRequest {
                state: state.as_ref().clone(),
                source,
                project_path: project_identity.to_path_buf(),
                principal_id: context.fingerprint.clone(),
                checkout_id: format!("bundle-release-source-{}", uuid::Uuid::new_v4()),
                pinned_realization: Some(
                    ryeos_executor::execution::project_source::PinnedContextRealization::ReadOnly,
                ),
                normalization: crate::routes::response_modes::execute_mode::ProjectRootNormalization::CanonicalizeLive,
                launch_timings: None,
            },
        )
        .await
        .map_err(|error| anyhow::anyhow!("resolve immutable release source: {error}"))?;
    anyhow::ensure!(
        project.snapshot_hash.as_deref() == Some(snapshot_hash),
        "release source hash differs from the authenticated project's current pushed snapshot"
    );
    let materialization = project
        .pinned_materialization
        .take()
        .context("release source resolver omitted pinned materialization authority")?;
    let generation = Arc::new(ReleaseSourceGeneration::from_materialization(
        materialization,
        &project.effective_path,
        project.original_path.clone(),
        snapshot_hash,
    )?);
    Ok((generation, project))
}

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
    catalog_namespace: String,
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
        // CasMutationGuard is deliberately thread-bound. Finish this read
        // phase before the asynchronous snapshot resolver and reacquire for
        // the subsequent synchronous measurement phase.
        drop(guard);
        let project = PathBuf::from(&request.project_path);
        let (source_generation, _source_context) = resolve_release_source_generation(
            &project,
            &calibration.source_snapshot_hash,
            &context,
            &state,
        )
        .await?;
        anyhow::ensure!(
            calibration.source_recipes == expected_calibration_recipes(&source_generation)?,
            "authority calibration measured another fixed recipe set"
        );
        let selected_environment =
            ryeos_app::bundle_publication::calibration::CalibrationEnvironmentSelection {
                python_runtime: calibration
                    .execution_environment
                    .python_runtime
                    .selection
                    .clone(),
                platform: calibration.execution_environment.platform.selection.clone(),
                static_link_inputs: calibration
                    .execution_environment
                    .static_link_inputs
                    .selection
                    .clone(),
                cargo_vendor: calibration
                    .execution_environment
                    .cargo_vendor
                    .selection
                    .clone(),
            };
        let current_environment = verified_calibration_environment(
            &selected_environment,
            &source_generation,
            &context,
            &state,
        )?;
        anyhow::ensure!(
            current_environment.python_runtime == calibration.execution_environment.python_runtime
                && current_environment.platform == calibration.execution_environment.platform
                && current_environment.static_link_inputs
                    == calibration.execution_environment.static_link_inputs
                && current_environment.cargo_vendor
                    == calibration.execution_environment.cargo_vendor
                && current_environment.terminal_uses
                    == calibration.execution_environment.terminal_uses,
            "calibrated execution environment is no longer currently admitted"
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
        let guard = authority.acquire_shared_guard()?;
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
                "calibration_run_attestation_hash": request.calibration_run_attestation_hash,
                "calibration_execution_environment": calibration.execution_environment,
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
    source_generation: &ReleaseSourceGeneration,
) -> anyhow::Result<ryeos_app::bundle_publication::calibration::AuthorityCalibrationRecipes> {
    use ryeos_app::bundle_publication::calibration::{
        AuthorityCalibrationRecipes, CalibrationRecipeIdentity,
    };
    let identity = |filename: &str| -> anyhow::Result<CalibrationRecipeIdentity> {
        let (signed, raw_content_digest) = source_generation.signed_recipe(&format!(
            "bundles/bundle-release/.ai/config/bundle-release/{filename}"
        ))?;
        Ok(CalibrationRecipeIdentity {
            signed_config_hash: sha256_bytes(signed.as_bytes()),
            raw_content_digest,
        })
    };
    Ok(AuthorityCalibrationRecipes {
        native_build: identity("calibration-portable-build-products.yaml")?,
        native_capture: identity("calibration-portable-capture-products.yaml")?,
        native_qualification: qualification_recipe_identity(
            source_generation,
            "portable-qualification.yaml",
        )?,
        core_seed_build: identity("calibration-core-build-products.yaml")?,
        core_seed_capture: identity("calibration-core-capture-products.yaml")?,
        core_seed_qualification: qualification_recipe_identity(
            source_generation,
            "core-seed-qualification.yaml",
        )?,
        substrate_build: identity("calibration-substrate-build-products.yaml")?,
        substrate_qualification: qualification_recipe_identity(
            source_generation,
            "substrate-qualification.yaml",
        )?,
    })
}

/// Author one run's exact producer allowance from a trusted, immutable source
/// template. Only the producer parameters change; ordinary Config admission
/// still verifies the resulting node signature and the product witness retains
/// the exact relationship.
fn calibration_invocation_recipe(
    source_generation: &ReleaseSourceGeneration,
    filename: &str,
    overlay_filename: &str,
    producer_ref: &str,
    parameters: &Value,
    state: &AppState,
) -> anyhow::Result<(String, String)> {
    let (source, _) = source_generation.signed_recipe(&format!(
        "bundles/bundle-release/.ai/config/bundle-release/{filename}"
    ))?;
    let source_body = verified_source_recipe_body(source_generation, &source, state)?;
    let body = calibration_invocation_recipe_body(
        &source_body,
        overlay_filename,
        producer_ref,
        parameters,
    )?;
    let signed = lillux::signature::sign_content(&body, state.identity.signing_key(), "#", None);
    Ok((signed, sha256_bytes(body.as_bytes())))
}

fn verified_source_recipe_body(
    source_generation: &ReleaseSourceGeneration,
    source: &str,
    state: &AppState,
) -> anyhow::Result<String> {
    let (signature_line, source_body) = source
        .split_once('\n')
        .context("release source template has no signature envelope")?;
    let signature = lillux::signature::parse_signature_line(signature_line, "#", None)
        .context("invalid release source template signature envelope")?;
    let trust = ryeos_runtime::verified_loader::TrustStore::load(
        source_generation.root(),
        &state.config.runtime_root().trusted_keys_dir(),
    )?;
    let signer = trust
        .get(&signature.signer_fingerprint)
        .context("release source template signer is not trusted")?;
    anyhow::ensure!(
        lillux::signature::is_valid_signature_for(
            &signature.content_hash,
            &signature.signature_b64,
            &signature.signer_fingerprint,
            source_body,
            &signer.verifying_key,
            &signer.fingerprint,
        ),
        "release source template signature is invalid"
    );
    Ok(source_body.to_owned())
}

fn calibration_invocation_recipe_body(
    source_body: &str,
    overlay_filename: &str,
    producer_ref: &str,
    parameters: &Value,
) -> anyhow::Result<String> {
    use ryeos_state::external_content::products::{
        ProductDeclarations, ProductRecipePurpose,
        admission::{AdmittedProductRecipeBinding, PRODUCT_RECIPE_BINDING_SCHEMA},
        composition::ProductRelationships,
    };
    let mut recipe: Value = serde_yaml::from_str(source_body)?;
    anyhow::ensure!(
        recipe["recipe_purpose"] == "authority_calibration_v1",
        "calibration template has another purpose"
    );
    let relationships = recipe["product_relationships"]["relationships"]
        .as_array_mut()
        .context("calibration template has no relationships")?;
    let mut matched = 0;
    for relationship in relationships {
        if relationship["producer"]["canonical_ref"] == producer_ref {
            anyhow::ensure!(
                relationship["producer"]["parameters"] == json!({}),
                "calibration template producer parameters are not empty"
            );
            relationship["producer"]["parameters"] = parameters.clone();
            matched += 1;
        }
    }
    anyhow::ensure!(matched > 0, "calibration template has no matching producer");
    let body = format!("{}\n", lillux::canonical_json(&recipe)?);
    anyhow::ensure!(
        body.len() <= 256 * 1024,
        "calibration recipe exceeds Config size bound"
    );
    let declarations = ProductDeclarations::from_value(recipe["build_products"].clone())?;
    let relationships: ProductRelationships =
        serde_json::from_value(recipe["product_relationships"].clone())?;
    AdmittedProductRecipeBinding {
        schema: PRODUCT_RECIPE_BINDING_SCHEMA.to_owned(),
        binding_name: "product_recipe".to_owned(),
        recipe_ref: format!(
            "config:bundle-release/{}",
            overlay_filename
                .strip_suffix(".yaml")
                .context("calibration overlay recipe is not YAML")?
        ),
        recipe_raw_content_digest: sha256_bytes(body.as_bytes()),
        purpose: ProductRecipePurpose::AuthorityCalibrationV1,
        declarations_hash: declarations.content_hash()?,
        declarations,
        relationships,
    }
    .validate()?;
    Ok(body)
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

fn environment_selection(
    declaration_id: &str,
    selected: &ryeos_app::bundle_publication::calibration::CalibrationProductSelection,
) -> ryeos_state::external_content::products::composition::ProductSelectionInput {
    ryeos_state::external_content::products::composition::ProductSelectionInput {
        target: ryeos_state::external_content::products::composition::ProductSelectionTarget::Root {},
        selection: ryeos_state::external_content::products::composition::ProductSelection {
            declaration_id: declaration_id.to_owned(),
            witness_hash: selected.product_witness_hash.clone(),
            witness_source: ryeos_state::external_content::products::transfer::ProductWitnessSource::LocalCapture {},
            qualification_hash: Some(selected.qualification_attestation_hash.clone()),
        },
    }
}

fn portable_environment_selections(
    environment: &ryeos_app::bundle_publication::calibration::CalibrationEnvironmentSelection,
) -> Vec<ryeos_state::external_content::products::composition::ProductSelectionInput> {
    vec![environment_selection("python", &environment.python_runtime)]
}

/// Build inputs do not become capture/verifier inputs. Those stages copy or
/// inspect the selected subject; they neither compile nor accept compiler slots.
struct CalibrationLaneEnvironment {
    build: ryeos_state::external_content::products::composition::ProductSelectionInputs,
    runtime: ryeos_state::external_content::products::composition::ProductSelectionInputs,
}

impl CalibrationLaneEnvironment {
    fn portable(
        environment: &ryeos_app::bundle_publication::calibration::CalibrationEnvironmentSelection,
    ) -> Self {
        Self {
            build: portable_environment_selections(environment),
            runtime: portable_environment_selections(environment),
        }
    }

    fn native(
        environment: &ryeos_app::bundle_publication::calibration::CalibrationEnvironmentSelection,
    ) -> Self {
        Self {
            build: native_environment_selections(environment),
            runtime: portable_environment_selections(environment),
        }
    }
}

fn qualified_platform_target(
    environment: &ryeos_app::bundle_publication::calibration::CalibrationEnvironmentProduct,
) -> anyhow::Result<ryeos_bundle_publication_contract::BundleTarget> {
    let evidence = environment
        .qualification_probe_evidence
        .as_object()
        .context("platform qualification evidence must be an object")?;
    let expected_top = [
        "abi_members",
        "compiler",
        "elf_closure_digest",
        "elf_count",
        "executable",
        "network_contacted",
        "required_abi_members",
        "required_executables",
        "runtime_entry_count",
        "runtime_inventory_digest",
        "runtime_total_bytes",
        "schema",
        "schema_version",
        "target",
    ]
    .into_iter()
    .collect::<std::collections::BTreeSet<_>>();
    anyhow::ensure!(
        evidence
            .keys()
            .map(String::as_str)
            .collect::<std::collections::BTreeSet<_>>()
            == expected_top,
        "platform qualification evidence fields are not the closed current schema"
    );
    anyhow::ensure!(
        evidence.get("schema").and_then(Value::as_str)
            == Some("ryeos.development_platform_evidence.v1")
            && evidence.get("schema_version").and_then(Value::as_u64) == Some(1)
            && evidence.get("network_contacted").and_then(Value::as_bool) == Some(false),
        "platform qualification evidence schema or network testimony is invalid"
    );
    let target = evidence
        .get("target")
        .and_then(Value::as_object)
        .context("platform qualification target is absent")?;
    anyhow::ensure!(
        target.len() == 4
            && target.get("triple").and_then(Value::as_str) == Some("x86_64-unknown-linux-gnu")
            && target.get("architecture").and_then(Value::as_str) == Some("x86_64")
            && target.get("operating_system").and_then(Value::as_str) == Some("linux")
            && target.get("abi").and_then(Value::as_str) == Some("gnu"),
        "platform qualification target is not the supported exact target"
    );
    for field in ["runtime_inventory_digest", "elf_closure_digest"] {
        let value = evidence
            .get(field)
            .and_then(Value::as_str)
            .context("platform evidence digest is absent")?;
        anyhow::ensure!(
            value.len() == 64
                && value
                    .bytes()
                    .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()),
            "platform evidence digest is invalid"
        );
    }
    for field in ["runtime_entry_count", "runtime_total_bytes", "elf_count"] {
        anyhow::ensure!(
            evidence
                .get(field)
                .and_then(Value::as_u64)
                .is_some_and(|value| value > 0),
            "platform evidence count is invalid"
        );
    }
    let exact_array = |field: &str, expected: &[&str]| -> anyhow::Result<()> {
        let actual = evidence
            .get(field)
            .and_then(Value::as_array)
            .context("platform evidence member list is absent")?;
        anyhow::ensure!(
            actual.len() == expected.len()
                && actual
                    .iter()
                    .zip(expected)
                    .all(|(a, b)| a.as_str() == Some(*b)),
            "platform evidence member list is not exact"
        );
        Ok(())
    };
    exact_array(
        "required_executables",
        &[
            "rust/bin/cargo",
            "rust/bin/rustc",
            "rust/bin/rustdoc",
            "native/bin/ar",
            "native/bin/collect2",
            "native/bin/gcc",
            "native/bin/ld.lld",
            "zig/zig",
        ],
    )?;
    exact_array(
        "required_abi_members",
        &[
            "lib/ld-linux-x86-64.so.2",
            "lib/libc.so.6",
            "lib/libc_nonshared.a",
            "lib/crtbeginS.o",
            "lib/crtendS.o",
            "lib/Scrt1.o",
            "lib/crti.o",
            "lib/crtn.o",
        ],
    )?;
    for field in ["compiler", "executable", "abi_members"] {
        anyhow::ensure!(
            evidence.get(field).is_some_and(Value::is_object),
            "platform typed evidence member is absent"
        );
    }
    Ok(ryeos_bundle_publication_contract::BundleTarget::Triple {
        triple: "x86_64-unknown-linux-gnu".to_owned(),
    })
}

fn native_environment_selections(
    environment: &ryeos_app::bundle_publication::calibration::CalibrationEnvironmentSelection,
) -> Vec<ryeos_state::external_content::products::composition::ProductSelectionInput> {
    vec![
        environment_selection("cargo-vendor", &environment.cargo_vendor),
        environment_selection("platform", &environment.platform),
        environment_selection("python", &environment.python_runtime),
        environment_selection("static-link-inputs", &environment.static_link_inputs),
    ]
}

fn calibration_source_bundle_roots(source_root: &Path) -> Vec<PathBuf> {
    vec![
        source_root.join("bundles/bundle-release"),
        source_root.join("bundles/standard"),
    ]
}

fn verified_calibration_product(
    selection: &ryeos_app::bundle_publication::calibration::CalibrationProductSelection,
    expected_recipe_ref: &str,
    expected_product_name: &str,
    expected_policy_ref: &str,
    loader: &ryeos_runtime::verified_loader::VerifiedLoader,
    context: &HandlerContext,
    state: &AppState,
) -> anyhow::Result<ryeos_app::bundle_publication::calibration::CalibrationEnvironmentProduct> {
    use ryeos_state::external_content::products::{
        ProductCaptureEvidence, qualification::ProductQualificationEvidence,
    };
    selection.validate()?;
    let authority = state.state_store.pinned_state_authority()?;
    let guard = authority.acquire_shared_guard()?;
    let limits = state
        .node_policy
        .require::<ryeos_app::node_policy::sections::object_closure::NodeObjectClosurePolicy>()?
        .closure_limits()?;
    let qualification_attestation = ryeos_state::objects::Attestation::from_value(
        &authority
            .cas_store()?
            .get_object(&selection.qualification_attestation_hash)?
            .context("selected environment qualification is absent")?,
    )?;
    let untrusted = ProductQualificationEvidence::from_value(&qualification_attestation.evidence)?;
    let owner_principal = untrusted.product_coordinate.owner_principal.clone();
    let measured = ryeos_app::operator_external_content::product_qualification::measure_current_qualification_authority(
        state,
        context,
        &authority,
        &guard,
        limits,
        &owner_principal,
        &selection.qualification_attestation_hash,
    )?;
    anyhow::ensure!(
        measured.qualified_product_witness_hash == selection.product_witness_hash
            && measured.qualified_product_owner_principal == owner_principal,
        "selected environment qualification names another product"
    );
    let evidence = ProductQualificationEvidence::verify_attestation_for_owner(
        &qualification_attestation,
        state.identity.verifying_key(),
        &owner_principal,
    )?;
    anyhow::ensure!(
        evidence.policy_source.canonical_ref == expected_policy_ref,
        "selected environment uses another qualification policy"
    );
    let witness = ryeos_state::objects::Attestation::from_value(
        &authority
            .cas_store()?
            .get_object(&selection.product_witness_hash)?
            .context("selected environment product witness is absent")?,
    )?;
    let captured = ProductCaptureEvidence::verify_attestation_for_owner(
        &witness,
        state.identity.verifying_key(),
        &owner_principal,
    )?;
    anyhow::ensure!(
        captured.recipe_ref == expected_recipe_ref
            && captured.declaration.name == expected_product_name,
        "selected environment product differs from its admitted recipe"
    );
    let recipe_id = expected_recipe_ref
        .strip_prefix("config:")
        .context("environment recipe must be a Config")?;
    let _recipe = loader
        .load_config_strict_signed_with_proof::<serde_json::Value>(recipe_id)?
        .context("current signed environment recipe is absent")?;
    let policy_id = expected_policy_ref
        .strip_prefix("config:")
        .context("environment qualification policy must be a Config")?;
    let policy = loader
        .load_config_strict_signed_with_proof::<serde_json::Value>(policy_id)?
        .context("current signed environment qualification policy is absent")?;
    anyhow::ensure!(
        measured.qualification_policy.raw_content_digest
            == evidence.policy_source.raw_content_digest
            && measured.qualification_policy.effective_definition_digest
                == evidence.policy_source.effective_definition_digest,
        "selected environment qualification policy is no longer current"
    );
    Ok(
        ryeos_app::bundle_publication::calibration::CalibrationEnvironmentProduct {
            selection: selection.clone(),
            owner_principal,
            producer: captured.producer,
            recipe_ref: captured.recipe_ref,
            recipe_raw_content_digest: captured.recipe_raw_content_digest,
            qualification_policy_item_hash: policy.dependency_proof.identity_digest()?,
            verifier_artifact_hash: ryeos_state::objects::canonical_value_digest(
                &serde_json::to_value(&measured.qualification_verifier_artifact_identity)?,
            )?,
            qualification_probe_evidence: evidence.result.probe_evidence,
        },
    )
}

fn verified_calibration_environment(
    selected: &ryeos_app::bundle_publication::calibration::CalibrationEnvironmentSelection,
    source_generation: &ReleaseSourceGeneration,
    context: &HandlerContext,
    state: &AppState,
) -> anyhow::Result<ryeos_app::bundle_publication::calibration::CalibrationEnvironmentEvidence> {
    use ryeos_app::bundle_publication::calibration::{
        CalibrationEnvironmentEvidence, CalibrationEnvironmentKind as Kind,
        CalibrationEnvironmentUse, CalibrationTerminal as Terminal,
    };
    selected.validate()?;
    let loader = ryeos_runtime::verified_loader::VerifiedLoader::new_with_node_config(
        source_generation.root().to_path_buf(),
        state.engine.node_config_root(),
        calibration_source_bundle_roots(source_generation.root()),
        &state.config.runtime_root().trusted_keys_dir(),
    )?;
    let python_runtime = verified_calibration_product(
        &selected.python_runtime,
        "config:development/ryeos/gnu-python-products",
        "runtime",
        "config:ryeos/environments/qualification/gnu-python",
        &loader,
        context,
        state,
    )?;
    let platform = verified_calibration_product(
        &selected.platform,
        "config:development/ryeos/platform-products",
        "platform",
        "config:ryeos/environments/qualification/development-platform",
        &loader,
        context,
        state,
    )?;
    let cargo_vendor = verified_calibration_product(
        &selected.cargo_vendor,
        "config:development/ryeos/cargo-vendor-products",
        "cargo_vendor",
        "config:ryeos/environments/qualification/cargo-vendor",
        &loader,
        context,
        state,
    )?;
    let static_link_inputs = verified_calibration_product(
        &selected.static_link_inputs,
        "config:development/ryeos/static-link-input-products",
        "static_link_inputs",
        "config:ryeos/environments/qualification/static-link-inputs",
        &loader,
        context,
        state,
    )?;
    let relationship = loader
        .load_config_strict_signed_with_proof::<serde_json::Value>(
            "bundle-release/execution-environment-products",
        )?
        .context("signed release environment relationships are absent")?;
    let relationship_identity = relationship.dependency_proof.identity_digest()?;
    let relationships =
        ryeos_state::external_content::products::composition::ProductRelationships::from_value(
            relationship
                .value
                .get("product_relationships")
                .cloned()
                .context("release environment Config omits product_relationships")?,
        )?;
    let uses = [
        (
            Terminal::PortableBuild,
            Kind::PythonRuntime,
            "python_to_portable_build",
        ),
        (
            Terminal::PortableCapture,
            Kind::PythonRuntime,
            "python_to_portable_signed_capture",
        ),
        (
            Terminal::PortableQualification,
            Kind::PythonRuntime,
            "python_to_portable_qualify",
        ),
        (
            Terminal::NativeBuild,
            Kind::PythonRuntime,
            "python_to_native_build",
        ),
        (
            Terminal::NativeBuild,
            Kind::Platform,
            "platform_to_native_build",
        ),
        (
            Terminal::NativeBuild,
            Kind::CargoVendor,
            "cargo_vendor_to_native_build",
        ),
        (
            Terminal::NativeBuild,
            Kind::StaticLinkInputs,
            "static_link_inputs_to_native_build",
        ),
        (
            Terminal::NativeCapture,
            Kind::PythonRuntime,
            "python_to_signed_capture",
        ),
        (
            Terminal::NativeQualification,
            Kind::PythonRuntime,
            "python_to_native_qualify",
        ),
        (
            Terminal::CoreSeedBuild,
            Kind::PythonRuntime,
            "python_to_core_seed_build",
        ),
        (
            Terminal::CoreSeedBuild,
            Kind::Platform,
            "platform_to_core_seed_build",
        ),
        (
            Terminal::CoreSeedBuild,
            Kind::CargoVendor,
            "cargo_vendor_to_core_seed_build",
        ),
        (
            Terminal::CoreSeedBuild,
            Kind::StaticLinkInputs,
            "static_link_inputs_to_core_seed_build",
        ),
        (
            Terminal::CoreSeedCapture,
            Kind::PythonRuntime,
            "python_to_core_seed_capture",
        ),
        (
            Terminal::CoreSeedQualification,
            Kind::PythonRuntime,
            "python_to_core_seed_qualify",
        ),
        (
            Terminal::SubstrateBuild,
            Kind::PythonRuntime,
            "python_to_substrate_build",
        ),
        (
            Terminal::SubstrateQualification,
            Kind::PythonRuntime,
            "python_to_substrate_qualify",
        ),
    ];
    for (terminal, kind, name) in uses {
        let entry = relationships
            .relationships
            .iter()
            .find(|entry| entry.name == name)
            .with_context(|| format!("release environment relationship {name} is absent"))?;
        let (product, product_name, declaration_id, consumer_ref, policy_ref) = match kind {
            Kind::PythonRuntime => (
                &python_runtime,
                "runtime",
                "python",
                match terminal {
                    Terminal::PortableBuild => {
                        ryeos_app::bundle_publication::recipe::PORTABLE_BUILD_GRAPH
                    }
                    Terminal::PortableCapture => {
                        ryeos_app::bundle_publication::recipe::PORTABLE_CAPTURE_GRAPH
                    }
                    Terminal::PortableQualification => {
                        ryeos_app::bundle_publication::recipe::PORTABLE_QUALIFIER
                    }
                    Terminal::NativeBuild => ryeos_app::bundle_publication::recipe::BUILD_GRAPH,
                    Terminal::NativeCapture => ryeos_app::bundle_publication::recipe::CAPTURE_GRAPH,
                    Terminal::NativeQualification => NATIVE_QUALIFY_TOOL_REF,
                    Terminal::CoreSeedBuild => {
                        ryeos_app::bundle_publication::core_seed::BUILD_GRAPH
                    }
                    Terminal::CoreSeedCapture => {
                        ryeos_app::bundle_publication::core_seed::CAPTURE_GRAPH
                    }
                    Terminal::CoreSeedQualification => {
                        ryeos_app::bundle_publication::core_seed::QUALIFIER
                    }
                    Terminal::SubstrateBuild => SUBSTRATE_BUILD_GRAPH_REF,
                    Terminal::SubstrateQualification => SUBSTRATE_QUALIFY_TOOL_REF,
                },
                "config:ryeos/environments/qualification/gnu-python",
            ),
            Kind::Platform => (
                &platform,
                "platform",
                "platform",
                match terminal {
                    Terminal::NativeBuild => ryeos_app::bundle_publication::recipe::BUILD_GRAPH,
                    Terminal::CoreSeedBuild => {
                        ryeos_app::bundle_publication::core_seed::BUILD_GRAPH
                    }
                    _ => anyhow::bail!("platform relationship grants an invalid terminal"),
                },
                "config:ryeos/environments/qualification/development-platform",
            ),
            Kind::StaticLinkInputs => (
                &static_link_inputs,
                "static_link_inputs",
                "static-link-inputs",
                match terminal {
                    Terminal::NativeBuild => ryeos_app::bundle_publication::recipe::BUILD_GRAPH,
                    Terminal::CoreSeedBuild => {
                        ryeos_app::bundle_publication::core_seed::BUILD_GRAPH
                    }
                    _ => anyhow::bail!("static-link relationship grants an invalid terminal"),
                },
                "config:ryeos/environments/qualification/static-link-inputs",
            ),
            Kind::CargoVendor => (
                &cargo_vendor,
                "cargo_vendor",
                "cargo-vendor",
                match terminal {
                    Terminal::NativeBuild => ryeos_app::bundle_publication::recipe::BUILD_GRAPH,
                    Terminal::CoreSeedBuild => {
                        ryeos_app::bundle_publication::core_seed::BUILD_GRAPH
                    }
                    _ => anyhow::bail!("Cargo vendor relationship grants an invalid terminal"),
                },
                "config:ryeos/environments/qualification/cargo-vendor",
            ),
        };
        anyhow::ensure!(
            entry.producer.canonical_ref == product.producer.canonical_ref
                && entry.producer.product_name == product_name
                && entry.producer.recipe_binding == "product_recipe"
                && entry.producer.admitted_parameters_digest()?
                    == product.producer.admitted_parameters_digest
                && entry.consumer.canonical_ref == consumer_ref
                && entry.consumer.declaration_id == declaration_id
                && entry.qualification.policy_ref.as_deref() == Some(policy_ref),
            "release environment relationship {name} does not exactly bind its selected product and terminal"
        );
    }
    let terminal_uses = uses
        .into_iter()
        .map(
            |(terminal, environment, relationship_name)| CalibrationEnvironmentUse {
                terminal,
                environment,
                relationship_name: relationship_name.to_owned(),
                relationship_config_item_hash: relationship_identity.clone(),
            },
        )
        .collect();
    let result = CalibrationEnvironmentEvidence {
        node_policy_generation_hash: state.node_policy.generation_digest().to_owned(),
        python_runtime,
        platform,
        cargo_vendor,
        static_link_inputs,
        terminal_uses,
    };
    result.require_selection(selected)?;
    Ok(result)
}

fn calibrated_catalog_environment(
    catalog: &ryeos_app::node_policy::sections::bundle_publication::BundleCatalogPolicy,
    source_generation: &ReleaseSourceGeneration,
    context: &HandlerContext,
    state: &AppState,
) -> anyhow::Result<ryeos_app::bundle_publication::calibration::CalibrationEnvironmentSelection> {
    use ryeos_app::bundle_publication::calibration::{
        AuthorityCalibrationEvidence, CalibrationEnvironmentSelection,
    };
    let authority = state.state_store.pinned_state_authority()?;
    let attestation = ryeos_state::objects::Attestation::from_value(
        &authority
            .cas_store()?
            .get_object(&catalog.calibration_run_attestation_hash)?
            .context("catalog authority calibration attestation is absent")?,
    )?;
    attestation.verify_with_key(state.identity.verifying_key())?;
    let calibration = AuthorityCalibrationEvidence::from_attestation(&attestation)?;
    anyhow::ensure!(
        calibration.execution_environment == catalog.calibration_execution_environment,
        "catalog execution environment differs from its signed calibration"
    );
    let selected = CalibrationEnvironmentSelection {
        static_link_inputs: catalog
            .calibration_execution_environment
            .static_link_inputs
            .selection
            .clone(),
        python_runtime: catalog
            .calibration_execution_environment
            .python_runtime
            .selection
            .clone(),
        platform: catalog
            .calibration_execution_environment
            .platform
            .selection
            .clone(),
        cargo_vendor: catalog
            .calibration_execution_environment
            .cargo_vendor
            .selection
            .clone(),
    };
    let current = verified_calibration_environment(&selected, source_generation, context, state)?;
    anyhow::ensure!(
        current.node_policy_generation_hash == state.node_policy.generation_digest(),
        "release environment was not measured under the active node policy generation"
    );
    anyhow::ensure!(
        catalog
            .calibration_execution_environment
            .node_policy_generation_hash
            != current.node_policy_generation_hash,
        "catalog calibration must precede the active policy generation that admits it"
    );
    anyhow::ensure!(
        catalog.calibration_execution_environment.python_runtime == current.python_runtime
            && catalog.calibration_execution_environment.platform == current.platform
            && catalog.calibration_execution_environment.cargo_vendor == current.cargo_vendor
            && catalog.calibration_execution_environment.static_link_inputs
                == current.static_link_inputs
            && catalog.calibration_execution_environment.terminal_uses == current.terminal_uses,
        "catalog execution environment is no longer admitted by current signed authority"
    );
    Ok(selected)
}

async fn run_authority_calibration(
    request: ryeos_app::bundle_publication::calibration::AuthorityCalibrationRequest,
    context: HandlerContext,
    state: Arc<AppState>,
) -> anyhow::Result<ryeos_app::bundle_publication::calibration::AuthorityCalibrationResult> {
    use ryeos_app::bundle_publication::{
        admitted_build::{
            CalibrationBundleInspectRequest, PayloadOwnershipConfigItem,
            inspect_calibration_bundle_input, inspect_calibration_core_input,
        },
        calibration::{
            AUTHORITY_CALIBRATION_SCHEMA, AuthorityCalibrationEvidence, AuthorityCalibrationRecipes,
        },
    };

    let project = PathBuf::from(&request.project_path);
    let (source_generation, _source_context) = resolve_release_source_generation(
        &project,
        &request.source_snapshot_hash,
        &context,
        &state,
    )
    .await?;
    let source_authority = source_generation.authority();
    let loader = ryeos_runtime::verified_loader::VerifiedLoader::new_with_node_config(
        source_generation.root().to_path_buf(),
        state.engine.node_config_root(),
        vec![source_generation.root().join("bundles/bundle-release")],
        &state.config.runtime_root().trusted_keys_dir(),
    )?;
    let ownership = loader
        .load_config_strict_signed_with_proof::<PayloadOwnershipConfigItem>(
            "bundle-release/payload-ownership",
        )?
        .context("required signed payload ownership config is absent")?
        .value
        .into_current()?;
    let execution_environment = verified_calibration_environment(
        &request.execution_environment,
        &source_generation,
        &context,
        &state,
    )?;
    let target = serde_json::to_value(qualified_platform_target(&execution_environment.platform)?)?;
    let native_input = inspect_calibration_bundle_input(
        &ownership,
        &source_authority,
        CalibrationBundleInspectRequest {
            project_path: source_generation.project_identity().display().to_string(),
            bundle_name: "bundle-release".to_owned(),
            source_snapshot_hash: request.source_snapshot_hash.clone(),
            target: json!({"kind":"portable"}),
            build_profile: "release".to_owned(),
        },
    )?;
    let core_input = inspect_calibration_core_input(
        &ownership,
        &source_authority,
        CalibrationBundleInspectRequest {
            project_path: source_generation.project_identity().display().to_string(),
            bundle_name: "core".to_owned(),
            source_snapshot_hash: request.source_snapshot_hash.clone(),
            target: target.clone(),
            build_profile: "release".to_owned(),
        },
    )?;

    let native = calibrate_signed_bundle(
        "bundle-release",
        ryeos_app::bundle_publication::recipe::PORTABLE_BUILD_GRAPH,
        ryeos_app::bundle_publication::recipe::PORTABLE_CAPTURE_GRAPH,
        ryeos_app::bundle_publication::recipe::PORTABLE_QUALIFIER,
        "portable_bundle",
        "unsigned_bundle",
        "signed_portable_bundle",
        "signed_portable_bundle_to_release_qualification",
        "calibration-portable-build-products.yaml",
        "portable-build-products.yaml",
        ryeos_app::bundle_publication::recipe::PORTABLE_BUILD_RECIPE_REF,
        "calibration-portable-capture-products.yaml",
        "portable-signed-capture-products.yaml",
        ryeos_app::bundle_publication::recipe::PORTABLE_CAPTURE_RECIPE_REF,
        CalibrationLaneEnvironment::portable(&request.execution_environment),
        native_input,
        &source_generation,
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
        CalibrationLaneEnvironment::native(&request.execution_environment),
        core_input,
        &source_generation,
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
        &source_generation,
        portable_environment_selections(&request.execution_environment),
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
        source_recipes: expected_calibration_recipes(&source_generation)?,
        recipes: AuthorityCalibrationRecipes {
            native_build: native.build,
            native_capture: native
                .capture
                .context("native calibration omitted capture recipe")?,
            native_qualification: qualification_recipe_identity(
                &source_generation,
                "portable-qualification.yaml",
            )?,
            core_seed_build: core.build,
            core_seed_capture: core
                .capture
                .context("Core calibration omitted capture recipe")?,
            core_seed_qualification: qualification_recipe_identity(
                &source_generation,
                "core-seed-qualification.yaml",
            )?,
            substrate_build: substrate.build,
            substrate_qualification: qualification_recipe_identity(
                &source_generation,
                "substrate-qualification.yaml",
            )?,
        },
        execution_environment,
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
    environment: CalibrationLaneEnvironment,
    release_input: Value,
    source_generation: &Arc<ReleaseSourceGeneration>,
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
    let build_selections = ryeos_state::external_content::products::composition::canonicalize_product_selection_inputs(
        environment.build,
    )?;
    let build_parameters = json!({
        "release_input": release_input.clone(),
        "child_product_selections": build_selections.clone(),
    });
    let (build_recipe, build_digest) = calibration_invocation_recipe(
        source_generation,
        build_source_filename,
        build_overlay_filename,
        build_graph,
        &build_parameters,
        &state,
    )?;
    let build_envelope = run_pinned_release_graph_from_generation(
        build_graph,
        Arc::clone(source_generation),
        build_recipe.clone(),
        build_overlay_filename,
        false,
        capture_overlay_filename,
        None,
        build_recipe_ref,
        build_digest.clone(),
        build_parameters.clone(),
        build_selections,
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
        build_recipe_ref,
        &build_digest,
        &build_parameters,
    )?;
    let signer = CalibrationCoreManifestAuthority::new(
        Arc::clone(&cas),
        state.identity.as_ref().clone(),
        Arc::new(source_generation.authority()),
    );
    let signed = signer.sign_and_capture(CalibrationCoreManifestRequest {
        project_path: source_generation.project_identity().display().to_string(),
        source_snapshot_hash: source_snapshot_hash.to_owned(),
        bundle_name: bundle_name.to_owned(),
        input_content_manifest_hash: build_manifest,
    })?;
    let signed_manifest = String::from_utf8(
        cas.get_blob(signed.evidence().output_manifest_item_hash())?
            .context("calibration signed manifest blob is absent")?,
    )
    .context("calibration signed manifest is not UTF-8")?;
    let mut selections = environment.runtime.clone();
    selections.push(ryeos_state::external_content::products::composition::ProductSelectionInput {
        target: ryeos_state::external_content::products::composition::ProductSelectionTarget::Root {},
        selection: ryeos_state::external_content::products::composition::ProductSelection {
            declaration_id: build_declaration.to_owned(),
            witness_hash: build_product.witness_hash.clone(),
            witness_source: ryeos_state::external_content::products::transfer::ProductWitnessSource::LocalCapture {},
            qualification_hash: None,
        },
    });
    let selections = ryeos_state::external_content::products::composition::canonicalize_product_selection_inputs(selections)?;
    let capture_parameters = json!({
        "release_input": release_input,
        "materialization_result_hash": signed.evidence_attestation_hash(),
        "signed_tree_manifest_hash": signed.evidence().output_content_manifest_hash(),
        "manifest_item_hash": signed.evidence().output_manifest_item_hash(),
        "signed_manifest": signed_manifest,
        "child_product_selections": selections.clone(),
    });
    let (capture_recipe, capture_digest) = calibration_invocation_recipe(
        source_generation,
        capture_source_filename,
        capture_overlay_filename,
        capture_graph,
        &capture_parameters,
        &state,
    )?;
    let capture_envelope = run_pinned_release_graph_from_generation(
        capture_graph,
        Arc::clone(source_generation),
        build_recipe.clone(),
        build_overlay_filename,
        false,
        capture_overlay_filename,
        Some(capture_recipe.clone()),
        capture_recipe_ref,
        capture_digest.clone(),
        capture_parameters.clone(),
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
        capture_recipe_ref,
        &capture_digest,
        &capture_parameters,
    )?;
    anyhow::ensure!(
        capture_manifest == signed.evidence().output_content_manifest_hash(),
        "{bundle_name} calibration capture changed the node-signed tree"
    );
    let verifier_chain_root_id = ryeos_app::thread_lifecycle::new_thread_id();
    let qualification_envelope = run_pinned_release_graph_from_generation(
        qualifier,
        Arc::clone(source_generation),
        build_recipe.clone(),
        build_overlay_filename,
        false,
        capture_overlay_filename,
        Some(capture_recipe.clone()),
        capture_recipe_ref,
        capture_digest.clone(),
        json!({}),
        {
            let mut selections = environment.runtime;
            selections.push(ryeos_state::external_content::products::composition::ProductSelectionInput {
            target: ryeos_state::external_content::products::composition::ProductSelectionTarget::Root {},
            selection: ryeos_state::external_content::products::composition::ProductSelection {
                declaration_id: "subject".to_owned(),
                witness_hash: capture_product.witness_hash.clone(),
                witness_source: ryeos_state::external_content::products::transfer::ProductWitnessSource::LocalCapture {},
                qualification_hash: None,
            },
            });
            selections
        },
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
    recipe_ref: &str,
    recipe_digest: &str,
    parameters: &Value,
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
    anyhow::ensure!(
        evidence.recipe_ref == recipe_ref
            && evidence.recipe_raw_content_digest == recipe_digest
            && evidence.root_producer.admitted_parameters_digest
                == ryeos_state::objects::canonical_value_digest(parameters)?,
        "calibration product witness differs from its exact run recipe and parameters"
    );
    Ok(evidence.manifest_hash)
}

fn sha256_bytes(bytes: &[u8]) -> String {
    use sha2::{Digest as _, Sha256};
    format!("{:x}", Sha256::digest(bytes))
}

fn qualification_recipe_identity(
    source_generation: &ReleaseSourceGeneration,
    filename: &str,
) -> anyhow::Result<ryeos_app::bundle_publication::calibration::CalibrationRecipeIdentity> {
    let (signed, raw_content_digest) = source_generation.signed_recipe(&format!(
        "bundles/bundle-release/.ai/config/bundle-release/{filename}"
    ))?;
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
    source_generation: &Arc<ReleaseSourceGeneration>,
    environment_selections: Vec<
        ryeos_state::external_content::products::composition::ProductSelectionInput,
    >,
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
    let environment_selections = ryeos_state::external_content::products::composition::canonicalize_product_selection_inputs(environment_selections)?;
    let build_parameters = json!({
        "receipt": receipt,
        "child_product_selections": environment_selections.clone(),
    });
    let (build_recipe, build_digest) = calibration_invocation_recipe(
        source_generation,
        "calibration-substrate-build-products.yaml",
        "substrate-build-products.yaml",
        SUBSTRATE_BUILD_GRAPH_REF,
        &build_parameters,
        &state,
    )?;
    let build_envelope = run_pinned_release_graph_from_generation(
        SUBSTRATE_BUILD_GRAPH_REF,
        Arc::clone(source_generation),
        build_recipe.clone(),
        "substrate-build-products.yaml",
        false,
        "signed-capture-products.yaml",
        None,
        "config:bundle-release/substrate-build-products",
        build_digest.clone(),
        build_parameters.clone(),
        environment_selections.clone(),
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
        "config:bundle-release/substrate-build-products",
        &build_digest,
        &build_parameters,
    )?;
    let verifier_chain_root_id = ryeos_app::thread_lifecycle::new_thread_id();
    let qualification_envelope = run_pinned_release_graph_from_generation(
        SUBSTRATE_QUALIFY_TOOL_REF,
        Arc::clone(source_generation),
        build_recipe.clone(),
        "substrate-build-products.yaml",
        false,
        "signed-capture-products.yaml",
        None,
        "config:bundle-release/substrate-build-products",
        build_digest.clone(),
        json!({}),
        {
            let mut selections = environment_selections;
            selections.push(ryeos_state::external_content::products::composition::ProductSelectionInput {
            target: ryeos_state::external_content::products::composition::ProductSelectionTarget::Root {},
            selection: ryeos_state::external_content::products::composition::ProductSelection {
                declaration_id: "subject".to_owned(),
                witness_hash: product.witness_hash.clone(),
                witness_source: ryeos_state::external_content::products::transfer::ProductWitnessSource::LocalCapture {},
                qualification_hash: None,
            },
            });
            selections
        },
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

#[allow(clippy::too_many_arguments)]
async fn run_pinned_release_graph_from_generation(
    graph_ref: &'static str,
    source_generation: Arc<ReleaseSourceGeneration>,
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
            source_generation,
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
        let project = PathBuf::from(&request.project_path);
        let (source_generation, _source_context) = resolve_release_source_generation(
            &project,
            &request.source_snapshot_hash,
            &context,
            &state,
        )
        .await?;
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
        let environment =
            calibrated_catalog_environment(catalog, &source_generation, &context, &state)?;
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
            anyhow::ensure!(
                receipt.target
                    == qualified_platform_target(
                        &catalog.calibration_execution_environment.platform
                    )?,
                "substrate receipt target differs from the currently qualified platform target"
            );
            receipt
        };
        let (signed_source_template, _) = source_generation.signed_recipe(
            "bundles/bundle-release/.ai/config/bundle-release/substrate-build-products.yaml",
        )?;
        verified_source_recipe_body(&source_generation, &signed_source_template, &state)?;
        let selections = ryeos_state::external_content::products::composition::canonicalize_product_selection_inputs(
            portable_environment_selections(&environment),
        )?;
        let recipe_request =
            ryeos_app::bundle_publication::recipe::AuthorizeSubstrateBuildRecipeRequest {
                catalog_namespace: request.catalog_namespace.clone(),
                bundle_publication_policy_section_digest: request
                    .bundle_publication_policy_section_digest
                    .clone(),
                trust_epoch: request.trust_epoch,
                receipt: receipt.clone(),
                child_product_selections: selections.clone(),
            };
        recipe_request.validate_source_template(&signed_source_template)?;
        let authorities = state
            .extensions
            .get::<BundleReleaseAuthorities>()
            .context("substrate build publisher authority is unavailable")?;
        let recipe = authorities
            .execute(BundleReleaseOperation::AuthorizeSubstrateBuildRecipe(
                recipe_request.clone(),
            ))
            .await?;
        recipe_request.validate_response(&recipe, &catalog.publisher_fingerprint)?;
        let signed_recipe = recipe["signed_config"]
            .as_str()
            .context("substrate recipe authorization omitted signed Config")?
            .to_owned();
        let recipe_digest = recipe["body_hash"]
            .as_str()
            .context("substrate recipe authorization omitted body identity")?
            .to_owned();
        let result = run_pinned_release_graph_from_generation(
            SUBSTRATE_BUILD_GRAPH_REF,
            Arc::clone(&source_generation),
            signed_recipe.clone(),
            "substrate-build-products.yaml",
            false,
            "signed-capture-products.yaml",
            None,
            "config:bundle-release/substrate-build-products",
            recipe_digest.clone(),
            recipe_request.parameters(),
            selections,
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
        context
            .require_verified()
            .map_err(|error| anyhow::anyhow!(error))?;
        let request: SubstrateQualifyRequest = crate::handler_error::parse_request(params)?;
        let (source_generation, _source_context) = resolve_release_source_generation(
            Path::new(&request.project_path),
            &request.source_snapshot_hash,
            &context,
            &state,
        )
        .await?;
        let policy = state.node_policy.require::<ryeos_app::node_policy::sections::bundle_publication::BundlePublicationPolicy>()?;
        let catalog = policy.require_catalog(&request.catalog_namespace)?;
        let (signed_source_template, _) = source_generation.signed_recipe(
            "bundles/bundle-release/.ai/config/bundle-release/substrate-build-products.yaml",
        )?;
        verified_source_recipe_body(&source_generation, &signed_source_template, &state)?;
        let (signature_line, recipe_body) = request
            .substrate_build_recipe_signed_config
            .split_once('\n')
            .context("substrate build recipe signature envelope is absent")?;
        let signature = lillux::signature::parse_signature_line(signature_line, "#", None)
            .context("invalid substrate build recipe signature envelope")?;
        anyhow::ensure!(
            signature.signer_fingerprint == catalog.publisher_fingerprint
                && signature.content_hash == request.substrate_build_recipe_raw_digest,
            "substrate build recipe signer or body identity differs from the current catalog"
        );
        let recipe_value: Value = serde_json::from_str(recipe_body)?;
        let producer_parameters =
            &recipe_value["product_relationships"]["relationships"][0]["producer"]["parameters"];
        let authorized =
            ryeos_app::bundle_publication::recipe::AuthorizeSubstrateBuildRecipeRequest {
                catalog_namespace: request.catalog_namespace.clone(),
                bundle_publication_policy_section_digest: policy.section_digest()?,
                trust_epoch: catalog.trust_epoch,
                receipt: serde_json::from_value(producer_parameters["receipt"].clone())?,
                child_product_selections: serde_json::from_value(
                    producer_parameters["child_product_selections"].clone(),
                )?,
            };
        authorized.validate_source_template(&signed_source_template)?;
        anyhow::ensure!(
            authorized.config_body()? == recipe_body
                && authorized.parameters() == *producer_parameters,
            "substrate build recipe is not the exact constrained publisher authorization"
        );
        let environment =
            calibrated_catalog_environment(catalog, &source_generation, &context, &state)?;
        let mut selections = portable_environment_selections(&environment);
        selections.push(ryeos_state::external_content::products::composition::ProductSelectionInput {
            target: ryeos_state::external_content::products::composition::ProductSelectionTarget::Root {},
            selection: ryeos_state::external_content::products::composition::ProductSelection {
                declaration_id: "subject".to_owned(),
                witness_hash: request.substrate_product_witness.clone(),
                witness_source: ryeos_state::external_content::products::transfer::ProductWitnessSource::LocalCapture {},
                qualification_hash: None,
            },
        });
        let verifier_chain_root_id = ryeos_app::thread_lifecycle::new_thread_id();
        let result = run_pinned_release_graph_from_generation(
            SUBSTRATE_QUALIFY_TOOL_REF,
            Arc::clone(&source_generation),
            request.substrate_build_recipe_signed_config,
            "substrate-build-products.yaml",
            false,
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
        anyhow::ensure!(
            request.child_product_selections.is_empty(),
            "Core seed service callers cannot provide internal child selectors"
        );
        let project = request.release_input["project_path"]
            .as_str()
            .context("Core seed release input has no project path")?;
        let source_hash = request.release_input["source_snapshot_hash"]
            .as_str()
            .context("Core seed release input has no source snapshot")?;
        let (source_generation, _source_context) =
            resolve_release_source_generation(Path::new(project), source_hash, &context, &state)
                .await?;
        let authored_manifest =
            ryeos_app::bundle_publication::admitted_build::materialize_release_manifest(
                source_generation.root(),
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
        let environment =
            calibrated_catalog_environment(catalog, &source_generation, &context, &state)?;
        let measured_target = serde_json::to_value(qualified_platform_target(
            &catalog.calibration_execution_environment.platform,
        )?)?;
        anyhow::ensure!(
            request.release_input.get("target") == Some(&measured_target),
            "Core seed target differs from the currently qualified platform target"
        );
        let selections = ryeos_state::external_content::products::composition::canonicalize_product_selection_inputs(
            native_environment_selections(&environment),
        )?;
        let mut recipe_build = request.clone();
        recipe_build.child_product_selections = selections.clone();
        let recipe_request =
            ryeos_app::bundle_publication::core_seed::CoreSeedRecipeRequest::Build(recipe_build);
        let graph_parameters = recipe_request.parameters();
        let authorities = state
            .extensions
            .get::<BundleReleaseAuthorities>()
            .context("bundle release publisher authority is unavailable")?;
        let recipe = authorities
            .execute(BundleReleaseOperation::AuthorizeCoreSeedRecipe(
                recipe_request.clone(),
            ))
            .await?;
        recipe_request.validate_response(&recipe, &catalog.publisher_fingerprint)?;
        let signed_recipe = recipe["signed_config"]
            .as_str()
            .context("Core seed recipe authorization omitted signed Config bytes")?
            .to_owned();
        let recipe_digest = recipe["body_hash"]
            .as_str()
            .context("Core seed recipe authorization omitted body identity")?
            .to_owned();
        let result = run_pinned_release_graph_from_generation(
            ryeos_app::bundle_publication::core_seed::BUILD_GRAPH,
            Arc::clone(&source_generation),
            signed_recipe.clone(),
            "core-seed-build-products.yaml",
            false,
            "core-seed-capture-products.yaml",
            None,
            ryeos_app::bundle_publication::core_seed::BUILD_RECIPE,
            recipe_digest.clone(),
            graph_parameters,
            selections,
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
        anyhow::ensure!(
            request.build.child_product_selections.is_empty(),
            "Core seed capture callers cannot provide internal child selectors"
        );
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
        let project = request.build.release_input["project_path"]
            .as_str()
            .context("Core seed release input has no project path")?;
        let source_hash = request.build.release_input["source_snapshot_hash"]
            .as_str()
            .context("Core seed release input has no source snapshot")?;
        let (source_generation, _source_context) =
            resolve_release_source_generation(Path::new(project), source_hash, &context, &state)
                .await?;
        let policy = state.node_policy.require::<ryeos_app::node_policy::sections::bundle_publication::BundlePublicationPolicy>()?;
        let catalog = policy.require_catalog(&request.build.catalog_namespace)?;
        let environment =
            calibrated_catalog_environment(catalog, &source_generation, &context, &state)?;
        let mut selections = portable_environment_selections(&environment);
        selections.push(ryeos_state::external_content::products::composition::ProductSelectionInput {
            target: ryeos_state::external_content::products::composition::ProductSelectionTarget::Root {},
            selection: ryeos_state::external_content::products::composition::ProductSelection {
                declaration_id: "unsigned_core".to_owned(),
                witness_hash: request.selected_product_witness,
                witness_source: ryeos_state::external_content::products::transfer::ProductWitnessSource::LocalCapture {},
                qualification_hash: None,
            },
        });
        let selections = ryeos_state::external_content::products::composition::canonicalize_product_selection_inputs(selections)?;
        let mut recipe_build = request.build.clone();
        recipe_build.child_product_selections = selections.clone();
        let recipe_request =
            ryeos_app::bundle_publication::core_seed::CoreSeedRecipeRequest::Capture(
                ryeos_app::bundle_publication::core_seed::CoreSeedCaptureRecipeRequest {
                    build: recipe_build,
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
                recipe_request.clone(),
            ))
            .await?;
        recipe_request.validate_response(&recipe, &catalog.publisher_fingerprint)?;
        let signed_capture_recipe = recipe["signed_config"]
            .as_str()
            .context("Core seed capture recipe omitted signed Config bytes")?
            .to_owned();
        let capture_recipe_digest = recipe["body_hash"]
            .as_str()
            .context("Core seed capture recipe omitted body identity")?
            .to_owned();
        let result = run_pinned_release_graph_from_generation(
            ryeos_app::bundle_publication::core_seed::CAPTURE_GRAPH,
            Arc::clone(&source_generation),
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
        let project = request.build.release_input["project_path"]
            .as_str()
            .context("Core seed release input has no project path")?;
        let source_hash = request.build.release_input["source_snapshot_hash"]
            .as_str()
            .context("Core seed release input has no source snapshot")?;
        let (source_generation, _source_context) =
            resolve_release_source_generation(Path::new(project), source_hash, &context, &state)
                .await?;
        let policy = state.node_policy.require::<ryeos_app::node_policy::sections::bundle_publication::BundlePublicationPolicy>()?;
        let catalog = policy.require_catalog(&request.build.catalog_namespace)?;
        let environment =
            calibrated_catalog_environment(catalog, &source_generation, &context, &state)?;
        let mut selections = portable_environment_selections(&environment);
        selections.push(ryeos_state::external_content::products::composition::ProductSelectionInput {
            target: ryeos_state::external_content::products::composition::ProductSelectionTarget::Root {},
            selection: ryeos_state::external_content::products::composition::ProductSelection {
                declaration_id: "subject".to_owned(),
                witness_hash: request.signed_product_witness.clone(),
                witness_source: ryeos_state::external_content::products::transfer::ProductWitnessSource::LocalCapture {},
                qualification_hash: None,
            },
        });
        let verifier_chain_root_id = ryeos_app::thread_lifecycle::new_thread_id();
        let result = run_pinned_release_graph_from_generation(
            ryeos_app::bundle_publication::core_seed::QUALIFIER,
            Arc::clone(&source_generation),
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
    context: HandlerContext,
    state: Arc<AppState>,
) -> Pin<Box<dyn Future<Output = anyhow::Result<Value>> + Send>> {
    Box::pin(async move {
        let request: InputInspectRequest = crate::handler_error::parse_request(params)?;
        let root = std::path::PathBuf::from(&request.project_path);
        let (source_generation, _source_context) = resolve_release_source_generation(
            &root,
            &request.source_snapshot_hash,
            &context,
            &state,
        )
        .await?;
        let policy = state.node_policy.require::<
            ryeos_app::node_policy::sections::bundle_publication::BundlePublicationPolicy,
        >()?;
        let catalog = policy.require_catalog(&request.catalog_namespace)?;
        anyhow::ensure!(!catalog.frozen, "bundle publication catalog is frozen");
        let ownership_loader =
            ryeos_runtime::verified_loader::VerifiedLoader::new_with_node_config(
                source_generation.root().to_path_buf(),
                state.engine.node_config_root(),
                vec![source_generation.root().join("bundles/bundle-release")],
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
            &source_generation.authority(),
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
    context: HandlerContext,
    state: Arc<AppState>,
) -> Pin<Box<dyn Future<Output = anyhow::Result<Value>> + Send>> {
    Box::pin(async move {
        let request: CoreSeedInspectRequest = crate::handler_error::parse_request(params)?;
        anyhow::ensure!(
            request.bundle_name == "core",
            "Core seed inspection requires core"
        );
        let root = PathBuf::from(&request.project_path);
        let (source_generation, _source_context) = resolve_release_source_generation(
            &root,
            &request.source_snapshot_hash,
            &context,
            &state,
        )
        .await?;
        let policy = state.node_policy.require::<
            ryeos_app::node_policy::sections::bundle_publication::BundlePublicationPolicy,
        >()?;
        let catalog = policy.require_catalog(&request.catalog_namespace)?;
        let loader = ryeos_runtime::verified_loader::VerifiedLoader::new_with_node_config(
            source_generation.root().to_path_buf(),
            state.engine.node_config_root(),
            vec![source_generation.root().join("bundles/bundle-release")],
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
            &source_generation.authority(),
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
        let (source_generation, _source_context) = resolve_release_source_generation(
            std::path::Path::new(project),
            source_hash,
            &context,
            &state,
        )
        .await?;
        let bundle_name = request.release_input["bundle_name"]
            .as_str()
            .context("release input has no bundle name")?;
        let authored_manifest =
            ryeos_app::bundle_publication::admitted_build::materialize_release_manifest(
                source_generation.root(),
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
        let admitted_input =
            ryeos_app::bundle_publication::admitted_build::AdmittedReleaseInput::from_value(
                &request.release_input,
            )?;
        let environment =
            calibrated_catalog_environment(catalog, &source_generation, &context, &state)?;
        let (build_recipe_filename, capture_recipe_filename, expected_product, selections) =
            if admitted_input.requires_binary_build {
                (
                    "native-build-products.yaml",
                    "signed-capture-products.yaml",
                    "native_bundle",
                    native_environment_selections(&environment),
                )
            } else {
                (
                    "portable-build-products.yaml",
                    "portable-signed-capture-products.yaml",
                    "portable_bundle",
                    portable_environment_selections(&environment),
                )
            };
        let selections = ryeos_state::external_content::products::composition::canonicalize_product_selection_inputs(selections)?;
        let recipe_request = ryeos_app::bundle_publication::recipe::AuthorizeBuildRecipeRequest {
            catalog_namespace: request.catalog_namespace.clone(),
            bundle_publication_policy_section_digest: request
                .bundle_publication_policy_section_digest
                .clone(),
            trust_epoch: request.trust_epoch,
            release_input: request.release_input.clone(),
            child_product_selections: selections.clone(),
        };
        let graph_ref = recipe_request.build_graph()?;
        let recipe_ref = recipe_request.canonical_ref()?;
        let graph_parameters = recipe_request.graph_parameters();
        let authorities = state
            .extensions
            .get::<BundleReleaseAuthorities>()
            .context("bundle release publisher authority is unavailable")?;
        let recipe = authorities
            .execute(BundleReleaseOperation::AuthorizeBuildRecipe(
                recipe_request.clone(),
            ))
            .await?;
        ryeos_app::bundle_publication::recipe::validate_recipe_response(
            &recipe_request,
            &recipe,
            &catalog.publisher_fingerprint,
        )?;
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
        let result = run_pinned_release_graph_from_generation(
            graph_ref,
            Arc::clone(&source_generation),
            signed_recipe.clone(),
            build_recipe_filename,
            false,
            capture_recipe_filename,
            None,
            recipe_ref,
            recipe_raw_digest.clone(),
            graph_parameters,
            selections,
            None,
            context.clone(),
            Arc::clone(&state),
        )
        .await?;
        let (accepted_hash, accepted) = accept_dispatch_products(&result, &state)?;
        anyhow::ensure!(
            accepted.producer_ref == graph_ref,
            "build result came from another producer"
        );
        let product = accepted
            .products
            .iter()
            .find(|product| product.product_name == expected_product)
            .context("build result omitted its declared bundle product")?;
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
        let (source_generation, _source_context) =
            resolve_release_source_generation(Path::new(project), source_hash, &context, &state)
                .await?;
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
        let admitted_input =
            ryeos_app::bundle_publication::admitted_build::AdmittedReleaseInput::from_value(
                &request.release_input,
            )?;
        let environment =
            calibrated_catalog_environment(catalog, &source_generation, &context, &state)?;
        let (build_recipe_filename, capture_recipe_filename, expected_product, mut selections) =
            if admitted_input.requires_binary_build {
                (
                    "native-build-products.yaml",
                    "signed-capture-products.yaml",
                    "signed_native_bundle",
                    portable_environment_selections(&environment),
                )
            } else {
                (
                    "portable-build-products.yaml",
                    "portable-signed-capture-products.yaml",
                    "signed_portable_bundle",
                    portable_environment_selections(&environment),
                )
            };
        selections.push(ryeos_state::external_content::products::composition::ProductSelectionInput {
            target: ryeos_state::external_content::products::composition::ProductSelectionTarget::Root {},
            selection: ryeos_state::external_content::products::composition::ProductSelection {
                declaration_id: "unsigned_bundle".to_owned(),
                witness_hash: request.selected_product_witness.clone(),
                witness_source: ryeos_state::external_content::products::transfer::ProductWitnessSource::LocalCapture {},
                qualification_hash: None,
            },
        });
        let selections = ryeos_state::external_content::products::composition::canonicalize_product_selection_inputs(selections)?;
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
                child_product_selections: selections.clone(),
            };
        let capture_graph = capture_recipe_request.capture_graph()?;
        let capture_recipe_ref = capture_recipe_request.canonical_ref()?;
        let graph_parameters = capture_recipe_request.graph_parameters();
        let authorities = state
            .extensions
            .get::<BundleReleaseAuthorities>()
            .context("bundle release publisher authority is unavailable")?;
        let recipe = authorities
            .execute(BundleReleaseOperation::AuthorizeCaptureRecipe(
                capture_recipe_request.clone(),
            ))
            .await?;
        ryeos_app::bundle_publication::recipe::validate_capture_recipe_response(
            &capture_recipe_request,
            &recipe,
            &catalog.publisher_fingerprint,
        )?;
        let signed_capture_recipe = recipe["signed_config"]
            .as_str()
            .context("publisher capture recipe omitted signed Config bytes")?
            .to_owned();
        let capture_recipe_digest = recipe["body_hash"]
            .as_str()
            .context("publisher capture recipe omitted body identity")?
            .to_owned();
        let result = run_pinned_release_graph_from_generation(
            capture_graph,
            Arc::clone(&source_generation),
            request.build_recipe_signed_config,
            build_recipe_filename,
            false,
            capture_recipe_filename,
            Some(signed_capture_recipe.clone()),
            capture_recipe_ref,
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
            accepted.producer_ref == capture_graph && accepted.products.len() == 1,
            "signed capture returned another producer or product set"
        );
        let product = &accepted.products[0];
        anyhow::ensure!(
            product.product_name == expected_product,
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
        let (source_generation, _source_context) =
            resolve_release_source_generation(Path::new(project), source_hash, &context, &state)
                .await?;
        let policy = state.node_policy.require::<
            ryeos_app::node_policy::sections::bundle_publication::BundlePublicationPolicy,
        >()?;
        let catalog = policy.require_catalog(&request.catalog_namespace)?;
        anyhow::ensure!(!catalog.frozen, "bundle publication catalog is frozen");
        let environment =
            calibrated_catalog_environment(catalog, &source_generation, &context, &state)?;
        let admitted_input =
            ryeos_app::bundle_publication::admitted_build::AdmittedReleaseInput::from_value(
                &request.release_input,
            )?;
        let (
            qualifier,
            build_recipe_filename,
            capture_recipe_filename,
            capture_recipe_ref,
            relationship,
            mut selections,
        ) = if admitted_input.requires_binary_build {
            (
                NATIVE_QUALIFY_TOOL_REF,
                "native-build-products.yaml",
                "signed-capture-products.yaml",
                ryeos_app::bundle_publication::recipe::CAPTURE_RECIPE_REF,
                "signed_native_bundle_to_release_qualification",
                portable_environment_selections(&environment),
            )
        } else {
            (
                ryeos_app::bundle_publication::recipe::PORTABLE_QUALIFIER,
                "portable-build-products.yaml",
                "portable-signed-capture-products.yaml",
                ryeos_app::bundle_publication::recipe::PORTABLE_CAPTURE_RECIPE_REF,
                "signed_portable_bundle_to_release_qualification",
                portable_environment_selections(&environment),
            )
        };
        selections.push(ryeos_state::external_content::products::composition::ProductSelectionInput {
            target: ryeos_state::external_content::products::composition::ProductSelectionTarget::Root {},
            selection: ryeos_state::external_content::products::composition::ProductSelection {
                declaration_id: "subject".to_owned(),
                witness_hash: request.signed_product_witness.clone(),
                witness_source: ryeos_state::external_content::products::transfer::ProductWitnessSource::LocalCapture {},
                qualification_hash: None,
            },
        });
        let verifier_chain_root_id = ryeos_app::thread_lifecycle::new_thread_id();
        let result = run_pinned_release_graph_from_generation(
            qualifier,
            Arc::clone(&source_generation),
            request.build_recipe_signed_config.clone(),
            build_recipe_filename,
            false,
            capture_recipe_filename,
            Some(request.capture_recipe_signed_config.clone()),
            capture_recipe_ref,
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
            relationship.to_owned(),
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
        "ryeos.execute.config.bundle-release/execution-environment-products",
        "ryeos.execute.config.bundle-release/execution-tool-products",
        "ryeos.execute.config.bundle-release/portable-build-products",
        "ryeos.execute.config.bundle-release/portable-qualification",
        "ryeos.execute.config.bundle-release/portable-signed-capture-products",
        "ryeos.execute.config.bundle-release/signed-capture-products",
        "ryeos.execute.config.bundle-release/substrate-build-products",
        "ryeos.execute.service.bundle-release/authority-calibrate",
        "ryeos.execute.tool.ryeos/bundle-release/core-seed-build",
        "ryeos.execute.tool.ryeos/bundle-release/core-seed-capture",
        "ryeos.execute.tool.ryeos/bundle-release/core-seed-qualify",
        "ryeos.execute.tool.ryeos/bundle-release/native-build",
        "ryeos.execute.tool.ryeos/bundle-release/native-qualify",
        "ryeos.execute.tool.ryeos/bundle-release/portable-build",
        "ryeos.execute.tool.ryeos/bundle-release/portable-qualify",
        "ryeos.execute.tool.ryeos/bundle-release/portable-signed-capture",
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
        "ryeos.execute.config.bundle-release/execution-environment-products",
        "ryeos.execute.config.bundle-release/execution-tool-products",
        "ryeos.execute.config.bundle-release/native-build-products",
        "ryeos.execute.config.bundle-release/portable-build-products",
        "ryeos.execute.service.bundle-release/generation-build",
        "ryeos.execute.tool.ryeos/bundle-release/native-build",
        "ryeos.execute.tool.ryeos/bundle-release/portable-build",
    ],
    handler: generation_build_handler,
};
pub const CORE_SEED_BUILD: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:bundle-release/core-seed-build",
    endpoint: "bundle_release.core_seed_build",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &[
        "ryeos.execute.config.bundle-release/core-seed-build-products",
        "ryeos.execute.config.bundle-release/execution-environment-products",
        "ryeos.execute.config.bundle-release/execution-tool-products",
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
        "ryeos.execute.config.bundle-release/execution-environment-products",
        "ryeos.execute.config.bundle-release/execution-tool-products",
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
        "ryeos.execute.config.bundle-release/execution-environment-products",
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
        "ryeos.execute.config.bundle-release/execution-environment-products",
        "ryeos.execute.config.bundle-release/execution-tool-products",
        "ryeos.execute.config.bundle-release/portable-signed-capture-products",
        "ryeos.execute.config.bundle-release/signed-capture-products",
        "ryeos.execute.service.bundle-release/generation-capture",
        "ryeos.execute.tool.ryeos/bundle-release/portable-signed-capture",
        "ryeos.execute.tool.ryeos/bundle-release/signed-capture",
    ],
    handler: generation_capture_handler,
};
pub const GENERATION_QUALIFY: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:bundle-release/generation-qualify",
    endpoint: "bundle_release.generation_qualify",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &[
        "ryeos.execute.config.bundle-release/execution-environment-products",
        "ryeos.execute.config.bundle-release/portable-qualification",
        "ryeos.execute.service.bundle-release/generation-qualify",
        "ryeos.execute.tool.ryeos/bundle-release/native-qualify",
        "ryeos.execute.tool.ryeos/bundle-release/portable-qualify",
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
        "ryeos.execute.config.bundle-release/execution-environment-products",
        "ryeos.execute.config.bundle-release/execution-tool-products",
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
        "ryeos.execute.config.bundle-release/execution-environment-products",
        "ryeos.execute.config.bundle-release/execution-tool-products",
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
    use std::path::PathBuf;

    #[test]
    fn calibration_invocation_recipes_bind_exact_parameters_and_keep_template_shape() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../../bundles/bundle-release/.ai/config/bundle-release");
        for (source, overlay, producer) in [
            (
                "calibration-portable-build-products.yaml",
                "portable-build-products.yaml",
                ryeos_app::bundle_publication::recipe::PORTABLE_BUILD_GRAPH,
            ),
            (
                "calibration-portable-capture-products.yaml",
                "portable-signed-capture-products.yaml",
                ryeos_app::bundle_publication::recipe::PORTABLE_CAPTURE_GRAPH,
            ),
            (
                "calibration-core-build-products.yaml",
                "core-seed-build-products.yaml",
                ryeos_app::bundle_publication::core_seed::BUILD_GRAPH,
            ),
            (
                "calibration-core-capture-products.yaml",
                "core-seed-capture-products.yaml",
                ryeos_app::bundle_publication::core_seed::CAPTURE_GRAPH,
            ),
            (
                "calibration-substrate-build-products.yaml",
                "substrate-build-products.yaml",
                SUBSTRATE_BUILD_GRAPH_REF,
            ),
        ] {
            let signed = std::fs::read_to_string(root.join(source)).unwrap();
            let (_, template_body) = signed.split_once('\n').unwrap();
            let parameters = json!({"exact_request":"run-1", "child_product_selections":[]});
            let derived =
                calibration_invocation_recipe_body(template_body, overlay, producer, &parameters)
                    .unwrap();
            let template: Value = serde_yaml::from_str(template_body).unwrap();
            let mut expected = template.clone();
            for relationship in expected["product_relationships"]["relationships"]
                .as_array_mut()
                .unwrap()
            {
                if relationship["producer"]["canonical_ref"] == producer {
                    relationship["producer"]["parameters"] = parameters.clone();
                }
            }
            let actual: Value = serde_json::from_str(&derived).unwrap();
            assert_eq!(
                actual, expected,
                "calibration template changed outside parameters: {source}"
            );
            assert_ne!(
                sha256_bytes(derived.as_bytes()),
                sha256_bytes(template_body.as_bytes())
            );
            assert!(
                calibration_invocation_recipe_body(
                    template_body,
                    overlay,
                    producer,
                    &json!({"exact_request":"x".repeat(20 * 1024)}),
                )
                .is_err()
            );
            assert!(
                calibration_invocation_recipe_body(
                    template_body,
                    overlay,
                    "graph:ryeos/bundle-release/unrelated",
                    &parameters,
                )
                .is_err()
            );
        }
    }

    #[test]
    fn static_inputs_are_selected_for_native_builds_only() {
        use ryeos_app::bundle_publication::calibration::{
            CalibrationEnvironmentSelection, CalibrationProductSelection,
        };
        let product = |digit: &str| CalibrationProductSelection {
            product_witness_hash: digit.repeat(64),
            qualification_attestation_hash: "f".repeat(64),
        };
        let environment = CalibrationEnvironmentSelection {
            python_runtime: product("1"),
            platform: product("2"),
            cargo_vendor: product("3"),
            static_link_inputs: product("4"),
        };
        let native = native_environment_selections(&environment);
        ryeos_state::external_content::products::composition::validate_product_selection_inputs(
            &native,
        )
        .unwrap();
        assert_eq!(native.len(), 4);
        assert_eq!(native[3].selection.declaration_id, "static-link-inputs");
        assert_eq!(native[3].selection.witness_hash, "4".repeat(64));
        assert_eq!(native[3].selection.qualification_hash, Some("f".repeat(64)));
        let portable = portable_environment_selections(&environment);
        assert_eq!(portable.len(), 1);
        assert_eq!(portable[0].selection.declaration_id, "python");

        let portable_lane = CalibrationLaneEnvironment::portable(&environment);
        let native_lane = CalibrationLaneEnvironment::native(&environment);
        let assets =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../bundles/bundle-release/.ai");
        for (item, selected, subject) in [
            (
                "graphs/ryeos/bundle-release/portable-build.yaml",
                &portable_lane.build,
                None,
            ),
            (
                "graphs/ryeos/bundle-release/portable-signed-capture.yaml",
                &portable_lane.runtime,
                Some("unsigned_bundle"),
            ),
            (
                "tools/ryeos/bundle-release/portable-qualify.yaml",
                &portable_lane.runtime,
                Some("subject"),
            ),
            (
                "graphs/ryeos/bundle-release/core-seed-build.yaml",
                &native_lane.build,
                None,
            ),
            (
                "graphs/ryeos/bundle-release/core-seed-capture.yaml",
                &native_lane.runtime,
                Some("unsigned_core"),
            ),
            (
                "tools/ryeos/bundle-release/core-seed-qualify.yaml",
                &native_lane.runtime,
                Some("subject"),
            ),
        ] {
            let definition: Value =
                serde_yaml::from_str(&std::fs::read_to_string(assets.join(item)).unwrap()).unwrap();
            let declared = definition["external_product_slots"]
                .as_array()
                .unwrap()
                .iter()
                .map(|slot| slot["id"].as_str().unwrap())
                .filter(|id| Some(*id) != subject)
                .collect::<std::collections::BTreeSet<_>>();
            let selected = selected
                .iter()
                .map(|input| input.selection.declaration_id.as_str())
                .collect::<std::collections::BTreeSet<_>>();
            assert_eq!(selected, declared, "wrong calibration inputs for {item}");
        }
        assert_eq!(native_lane.runtime, portable_lane.runtime);
        assert_eq!(
            native_lane.runtime[0].selection.witness_hash,
            "1".repeat(64)
        );
    }

    #[test]
    fn calibration_source_exposes_release_and_qualification_policy_owners() {
        let root = PathBuf::from("/retained/source");
        assert_eq!(
            calibration_source_bundle_roots(&root),
            vec![
                root.join("bundles/bundle-release"),
                root.join("bundles/standard"),
            ]
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

    #[test]
    fn compiled_release_capabilities_match_signed_services() {
        let service_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../../bundles/bundle-release/.ai/services/bundle-release");
        for descriptor in [
            AUTHORITY_CALIBRATE,
            CORE_SEED_BUILD,
            CORE_SEED_CAPTURE,
            CORE_SEED_QUALIFY,
            GENERATION_BUILD,
            GENERATION_CAPTURE,
            GENERATION_QUALIFY,
            SUBSTRATE_BUILD,
            SUBSTRATE_QUALIFY,
        ] {
            let service_name = descriptor
                .service_ref
                .strip_prefix("service:bundle-release/")
                .expect("release service ref");
            let source = std::fs::read_to_string(service_root.join(format!("{service_name}.yaml")))
                .expect("read signed release service");
            let service: serde_yaml::Value =
                serde_yaml::from_str(&source).expect("parse signed release service");
            let mut signed = service["required_caps"]
                .as_sequence()
                .expect("required_caps sequence")
                .iter()
                .map(|value| value.as_str().expect("capability string"))
                .collect::<Vec<_>>();
            let mut compiled = descriptor.required_caps.to_vec();
            signed.sort_unstable();
            compiled.sort_unstable();
            assert_eq!(compiled, signed, "capability drift for {service_name}");
        }
    }
}
