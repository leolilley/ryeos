//! Native bundle producer service endpoints.

use std::{collections::BTreeMap, future::Future, io::Read as _, pin::Pin, sync::Arc};

use anyhow::Context as _;
use base64::Engine as _;
use ryeos_app::{
    bundle_publication::producer::{
        BundleReleaseAuthorities, BundleReleaseOperation, CatalogRequestPublicationRequest,
        GenerationBuildRequest, GenerationCaptureRequest, GenerationFinalizeRequest,
        GenerationQualifyRequest, InputInspectRequest, RequestAuthorizationRequest,
        RequestTreeSigningRequest, SetComposeRequest, StatusRequest, SubmitRequest,
    },
    handler_context::HandlerContext,
    service_registry::ServiceDescriptor,
    state::AppState,
};
use ryeos_executor::executor::ServiceAvailability;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

const RELEASE_GRAPH_REF: &str = "graph:ryeos/bundle-release/publish";
const NATIVE_BUILD_GRAPH_REF: &str = "graph:ryeos/bundle-release/native-build";
const NATIVE_QUALIFY_GRAPH_REF: &str = "graph:ryeos/bundle-release/native-qualify";
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
    context
        .require_verified()
        .map_err(|error| anyhow::anyhow!(error))?;
    let request: CatalogRemotePublishRequest = crate::handler_error::parse_request(params)?;
    let policy = state
        .node_policy
        .require::<ryeos_app::node_policy::sections::bundle_publication::BundlePublicationPolicy>(
    )?;
    let catalog = policy.require_catalog(&request.catalog_namespace)?;
    anyhow::ensure!(
        context.fingerprint == catalog.publisher_fingerprint
            && state.identity.fingerprint() == catalog.publisher_fingerprint,
        "remote catalog publication must retain the exact configured publisher principal"
    );
    // Publication destinations are operator-configured node remotes. Release
    // source content must not be able to redirect publisher-authenticated
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
    context: &'a HandlerContext,
    authority: ryeos_state::PinnedStateAuthority,
    guard: ryeos_state::CasMutationGuard,
    limits: ryeos_state::object_closure::ObjectClosureLimits,
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
    fn verify_release_evidence(
        &self,
        generation: &ryeos_bundle_publication_contract::BundleGeneration,
        accepted: &ryeos_state::external_content::products::accepted_result::ProductBuildAcceptedResult,
        materialization: &ryeos_bundle_publication_contract::PublisherMaterializationResult,
        binding: &ryeos_app::bundle_publication::ReleasePolicyBinding,
    ) -> anyhow::Result<()> {
        let policy = self.state.node_policy.require::<ryeos_app::node_policy::sections::bundle_publication::BundlePublicationPolicy>()?;
        anyhow::ensure!(
            policy.section_digest()? == binding.bundle_publication_policy_section_digest,
            "bundle-publication policy digest is stale"
        );
        let catalog = policy.require_catalog(&binding.catalog_namespace)?;
        anyhow::ensure!(
            !catalog.frozen && catalog.trust_epoch == binding.trust_epoch,
            "catalog trust epoch is stale or frozen"
        );
        anyhow::ensure!(
            materialization.publisher_fingerprint == catalog.publisher_fingerprint,
            "materialization publisher is not authorized for catalog"
        );
        anyhow::ensure!(
            generation.provenance_hash.is_none() && generation.sbom_hash.is_none(),
            "unsupported release evidence must not be silently admitted"
        );
        anyhow::ensure!(
            generation.qualification_evidence_hashes.len() == 1,
            "native bundle release requires exactly one qualification"
        );
        let required = vec!["native_bundle_release_checks_v1".to_owned()];
        ryeos_app::operator_external_content::product_qualification::verify_current_qualification_for_release(
            self.state, self.context, &self.authority, &self.guard, self.limits,
            &accepted.owner_principal, &generation.qualification_evidence_hashes[0],
            &generation.content_manifest_hash, &required,
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

async fn run_authenticated_graph(
    graph_ref: &'static str,
    project_path: std::path::PathBuf,
    parameters: Value,
    product_selections: Vec<
        ryeos_state::external_content::products::composition::ProductSelectionInput,
    >,
    pre_minted_thread_id: Option<String>,
    context: HandlerContext,
    state: Arc<AppState>,
) -> anyhow::Result<Value> {
    anyhow::ensure!(
        context.verified,
        "bundle release execution requires an authenticated caller"
    );
    ryeos_app::execution_policy::authorize_standard_local_live_execution(&context.scopes)?;
    let project_path = project_path.canonicalize()?;
    let resolved_authority = ryeos_app::execution_policy::resolve_standard_local_live_authority(
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
        subject_resolution_authority: ryeos_engine::contracts::SubjectResolutionAuthority::LiveFs,
        current_site_id: site_id.clone(),
        origin_site_id: context.execution_origin(&site_id),
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
    let acting_principal = context.fingerprint.clone();
    let request = ryeos_executor::dispatch::DispatchRequest {
        launch_mode: "wait",
        target_site_id: None,
        validate_only: false,
        params: parameters,
        ref_bindings: Default::default(),
        product_selections,
        acting_principal: &acting_principal,
        project_path: &project_path,
        provenance,
        lifecycle_authority: resolved_authority.lifecycle,
        launch_timings: None,
        original_root_kind: "graph",
        pre_minted_thread_id,
        usage_subject: None,
        usage_subject_asserted_by: None,
        previous_thread_id: None,
        root_admission: None,
        root_dispatch_evidence: None,
        parent_execution_context: None,
        effect_authority: None,
    };
    ryeos_executor::dispatch::dispatch_with_handler_context(
        graph_ref, context, &request, &exec_ctx, &state,
    )
    .await
    .map_err(|error| anyhow::anyhow!("{graph_ref} dispatch failed: {error}"))
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
        let result = run_authenticated_graph(
            NATIVE_BUILD_GRAPH_REF,
            project.into(),
            serde_json::to_value(&request)?,
            Vec::new(),
            None,
            context.clone(),
            Arc::clone(&state),
        )
        .await?;
        let accepted = ryeos_state::external_content::products::accepted_result::ProductBuildAcceptedResult::from_value(&result)?;
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
        let accepted_hash = cas.store_object(&accepted.to_value()?)?;
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
            "build_execution_evidence_hash":accepted.producer_partition_identity
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
        let release_input_digest =
            lillux::sha256_hex(lillux::canonical_json(&request.release_input)?.as_bytes());
        let selections = vec![ryeos_state::external_content::products::composition::ProductSelectionInput {
            target: ryeos_state::external_content::products::composition::ProductSelectionTarget::Root {},
            selection: ryeos_state::external_content::products::composition::ProductSelection {
                declaration_id: "subject".to_owned(),
                witness_hash: request.manifest_item_hash.clone(),
                witness_source: ryeos_state::external_content::products::transfer::ProductWitnessSource::LocalCapture {},
                qualification_hash: None,
            },
        }];
        let verifier_chain_root_id = ryeos_app::thread_lifecycle::new_thread_id();
        let result = run_authenticated_graph(
            NATIVE_QUALIFY_GRAPH_REF,
            project.into(),
            json!({
                "release_input":request.release_input,
                "release_input_digest":release_input_digest,
                "captured_tree_manifest_hash":request.captured_tree_manifest_hash,
                "manifest_item_hash":request.manifest_item_hash,
            }),
            selections,
            Some(verifier_chain_root_id.clone()),
            context.clone(),
            Arc::clone(&state),
        )
        .await?;
        anyhow::ensure!(
            result.get("schema").and_then(Value::as_str)
                == Some("ryeos.product_qualification_result.v1"),
            "qualification returned another contract"
        );
        let probe = result
            .get("probe_evidence")
            .context("qualification omitted probe evidence")?;
        anyhow::ensure!(
            probe.get("release_input_digest").and_then(Value::as_str)
                == Some(&release_input_digest),
            "qualification changed release input"
        );
        anyhow::ensure!(
            probe
                .get("captured_tree_manifest_hash")
                .and_then(Value::as_str)
                == Some(&request.captured_tree_manifest_hash),
            "qualification changed captured tree"
        );
        anyhow::ensure!(
            probe.get("manifest_item_hash").and_then(Value::as_str)
                == Some(&request.manifest_item_hash),
            "qualification changed manifest item"
        );
        let children = state
            .state_store
            .list_thread_children(&verifier_chain_root_id)?;
        let mut verifiers = children.iter().filter(|child| {
            child.item_ref == "tool:ryeos/bundle-release/native-qualify"
                && child.status == "completed"
        });
        let verifier = verifiers
            .next()
            .context("qualification graph retained no completed verifier child")?;
        anyhow::ensure!(
            verifiers.next().is_none(),
            "qualification graph retained more than one verifier child"
        );
        let publication =
            ryeos_app::operator_external_content::product_qualification::qualify(
                Arc::clone(&state),
                context,
                ryeos_app::operator_external_content::product_qualification::ProductQualificationRequest {
                    witness_hash: request.manifest_item_hash,
                    witness_source: ryeos_state::external_content::products::transfer::ProductWitnessSource::LocalCapture {},
                    relationship_name: "native_bundle_to_release_qualification".to_owned(),
                    verifier_chain_root_id,
                    verifier_thread_id: verifier.thread_id.clone(),
                },
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
    context: &'a HandlerContext,
) -> anyhow::Result<ContextualReleaseProof<'a>> {
    let authority = state.state_store.pinned_state_authority()?;
    let cas = authority.cas_store()?;
    let guard = ryeos_state::CasMutationGuard::shared_from_cas_root(cas.root())?;
    authority.ensure_guard(&guard)?;
    let limits = state
        .node_policy
        .require::<ryeos_app::node_policy::sections::object_closure::NodeObjectClosurePolicy>()?
        .closure_limits()?;
    Ok(ContextualReleaseProof {
        state,
        context,
        authority,
        guard,
        limits,
    })
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

fn set_compose_handler(
    params: Value,
    context: HandlerContext,
    state: Arc<AppState>,
) -> Pin<Box<dyn Future<Output = anyhow::Result<Value>> + Send>> {
    Box::pin(async move {
        let request: SetComposeRequest = crate::handler_error::parse_request(params)?;
        BundleReleaseOperation::SetCompose(request.clone()).validate()?;
        let proof = contextual_proof(&state, &context)?;
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
pub const GENERATION_BUILD: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:bundle-release/generation-build",
    endpoint: "bundle_release.generation_build",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.bundle-release/generation-build"],
    handler: generation_build_handler,
};
descriptor!(
    REQUEST_TREE_SIGNING,
    "service:bundle-release/request-tree-signing",
    "bundle_release.request_tree_signing",
    "ryeos.execute.service.bundle-release/request-tree-signing",
    RequestTreeSigningRequest,
    BundleReleaseOperation::RequestTreeSigning
);
descriptor!(
    GENERATION_CAPTURE,
    "service:bundle-release/generation-capture",
    "bundle_release.generation_capture",
    "ryeos.execute.service.bundle-release/generation-capture",
    GenerationCaptureRequest,
    BundleReleaseOperation::GenerationCapture
);
pub const GENERATION_QUALIFY: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:bundle-release/generation-qualify",
    endpoint: "bundle_release.generation_qualify",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.bundle-release/generation-qualify"],
    handler: generation_qualify_handler,
};
pub const GENERATION_FINALIZE: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:bundle-release/generation-finalize",
    endpoint: "bundle_release.generation_finalize",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.bundle-release/generation-finalize"],
    handler: generation_finalize_handler,
};
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
    INPUT_INSPECT,
    GENERATION_BUILD,
    REQUEST_TREE_SIGNING,
    GENERATION_CAPTURE,
    GENERATION_QUALIFY,
    GENERATION_FINALIZE,
    REQUEST_AUTHORIZATION,
    SET_COMPOSE,
    CATALOG_REQUEST_PUBLICATION,
    CATALOG_REMOTE_PUBLISH,
    SUBMIT,
    STATUS,
];
