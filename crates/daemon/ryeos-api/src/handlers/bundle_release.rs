//! Native bundle producer service endpoints.

use std::{
    collections::BTreeMap,
    future::Future,
    io::Read as _,
    path::{Path, PathBuf},
    pin::Pin,
    process::{Command, Stdio},
    sync::Arc,
};

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
use sha2::{Digest as _, Sha256};

const RELEASE_GRAPH_REF: &str = "graph:ryeos/bundle-release/publish";
const NATIVE_BUILD_GRAPH_REF: &str = "graph:ryeos/bundle-release/native-build";
const SIGNED_CAPTURE_GRAPH_REF: &str = "graph:ryeos/bundle-release/signed-capture";
const NATIVE_QUALIFY_TOOL_REF: &str = "tool:ryeos/bundle-release/native-qualify";
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
    context: &'a HandlerContext,
    authority: ryeos_state::PinnedStateAuthority,
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
        _accepted: &ryeos_state::external_content::products::accepted_result::ProductBuildAcceptedResult,
        accepted_capture: &ryeos_state::external_content::products::accepted_result::ProductBuildAcceptedResult,
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
        // Acquire on the synchronous caller's thread. The proof object itself
        // never owns a thread-affine guard and remains Send + Sync.
        let guard = self.authority.acquire_shared_guard()?;
        ryeos_app::operator_external_content::product_qualification::verify_current_qualification_for_release(
            self.state, self.context, &self.authority, &guard, self.limits,
            &accepted_capture.owner_principal, &generation.qualification_evidence_hashes[0],
            &generation.selected_signed_product_witness, &generation.content_manifest_hash,
            &required,
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

fn materialize_release_execution_project(
    source: &Path,
    expected_source_hash: &str,
    signed_build_recipe: &str,
    signed_capture_recipe: Option<&str>,
) -> anyhow::Result<tempfile::TempDir> {
    struct HashingReader<R> {
        inner: R,
        digest: Sha256,
    }
    impl<R: std::io::Read> std::io::Read for HashingReader<R> {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            let count = self.inner.read(buffer)?;
            self.digest.update(&buffer[..count]);
            Ok(count)
        }
    }
    let workspace = tempfile::Builder::new()
        .prefix("ryeos-bundle-release-")
        .tempdir()?;
    let mut archive = Command::new("git")
        .args(["archive", "--format=tar", "HEAD"])
        .current_dir(source)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("start exact release source materialization")?;
    let stdout = archive
        .stdout
        .take()
        .context("release source materializer has no stdout")?;
    let mut reader = HashingReader {
        inner: stdout,
        digest: Sha256::new(),
    };
    {
        let mut tar = tar::Archive::new(&mut reader);
        tar.unpack(workspace.path())
            .context("materialize exact release source archive")?;
    }
    std::io::copy(&mut reader, &mut std::io::sink())
        .context("finish hashing exact release source archive")?;
    let output = archive
        .wait_with_output()
        .context("wait for release source materialization")?;
    anyhow::ensure!(
        output.status.success(),
        "release source materialization failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let materialized_source_hash = format!("{:x}", reader.digest.finalize());
    anyhow::ensure!(
        materialized_source_hash == expected_source_hash,
        "release source HEAD changed after its admitted snapshot was authorized"
    );
    let mut directory = workspace.path().to_path_buf();
    for component in [".ai", "config", "bundle-release"] {
        directory.push(component);
        match std::fs::symlink_metadata(&directory) {
            Ok(metadata) => anyhow::ensure!(
                metadata.is_dir() && !metadata.file_type().is_symlink(),
                "release recipe overlay crosses an unsafe source entry"
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                std::fs::create_dir(&directory)?;
            }
            Err(error) => return Err(error.into()),
        }
    }
    for (name, contents) in [
        ("native-build-products.yaml", Some(signed_build_recipe)),
        ("signed-capture-products.yaml", signed_capture_recipe),
    ] {
        let Some(contents) = contents else { continue };
        let recipe_path = directory.join(name);
        if let Ok(metadata) = std::fs::symlink_metadata(&recipe_path) {
            anyhow::ensure!(
                metadata.is_file() && !metadata.file_type().is_symlink(),
                "release source has an unsafe fixed recipe entry"
            );
        }
        lillux::atomic_write_private(&recipe_path, contents.as_bytes())?;
    }
    Ok(workspace)
}

async fn run_pinned_release_graph(
    graph_ref: &'static str,
    source_project: PathBuf,
    expected_source_hash: String,
    signed_build_recipe: String,
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
    anyhow::ensure!(
        context.verified,
        "bundle release execution requires an authenticated caller"
    );
    let workspace = tokio::task::spawn_blocking(move || {
        materialize_release_execution_project(
            &source_project,
            &expected_source_hash,
            &signed_build_recipe,
            signed_capture_recipe.as_deref(),
        )
    })
    .await
    .context("release execution project materializer stopped")??;
    let mut policy = ryeos_app::execution_policy::ExecutionPolicy::local_pinned_capture(
        ryeos_app::execution_policy::ExecutionResponse::Wait,
    );
    policy.ownership = ryeos_app::execution_policy::ExecutionOwnership::RequestScoped;
    policy.recovery = ryeos_app::execution_policy::ExecutionRecovery::None;
    policy.validate()?;
    let project_source =
        ryeos_executor::execution::project_source::ProjectSource::CaptureLiveFullProject;
    let mut project_ctx = crate::routes::response_modes::execute_mode::resolve_project_context_off_thread(
        crate::routes::response_modes::execute_mode::ResolveProjectContextRequest {
            state: state.as_ref().clone(),
            source: project_source.clone(),
            project_path: workspace.path().to_path_buf(),
            principal_id: context.fingerprint.clone(),
            checkout_id: format!("bundle-release-{}", uuid::Uuid::new_v4()),
            pinned_realization: Some(
                ryeos_executor::execution::project_source::PinnedContextRealization::Cow,
            ),
            normalization:
                crate::routes::response_modes::execute_mode::ProjectRootNormalization::CanonicalizeLive,
            launch_timings: None,
        },
    )
    .await
    .map_err(|error| anyhow::anyhow!("capture release execution project: {error}"))?;
    let resolved = crate::routes::response_modes::execute_mode::resolve_execution_contract(
        &policy,
        &project_source,
        &project_ctx,
        None,
        None,
        &context.fingerprint,
        &context.scopes,
        &state,
    )?;
    let effective_path = project_ctx.effective_path.clone();
    let site_id = state.threads.site_id().to_owned();
    let plan_ctx = ryeos_engine::contracts::PlanContext {
        requested_by: ryeos_engine::contracts::EffectivePrincipal::Local(
            ryeos_engine::contracts::Principal {
                fingerprint: context.fingerprint.clone(),
                scopes: context.scopes.clone(),
            },
        ),
        project_context: ryeos_engine::contracts::ProjectContext::LocalPath {
            path: effective_path.clone(),
        },
        subject_resolution_authority: resolved.provenance.subject_resolution_authority(),
        current_site_id: site_id.clone(),
        origin_site_id: context.execution_origin(&site_id),
        execution_hints: Default::default(),
        scheduled_fire: None,
        validate_only: false,
    };
    let recipe_ref = ryeos_engine::canonical_ref::CanonicalRef::parse(recipe_ref)?;
    let resolved_recipe = project_ctx.request_engine.resolve(&plan_ctx, &recipe_ref)?;
    anyhow::ensure!(
        resolved_recipe.raw_content_digest == expected_recipe_raw_digest,
        "admitted native build recipe differs from publisher-authorized bytes"
    );
    project_ctx
        .request_engine
        .verify(&plan_ctx, resolved_recipe)
        .context("publisher-authorized native build recipe is not trusted")?;
    let exec_ctx = ryeos_executor::executor::ExecutionContext {
        principal_fingerprint: context.fingerprint.clone(),
        caller_scopes: context.scopes.clone(),
        engine: Arc::clone(&project_ctx.request_engine),
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
        project_path: &effective_path,
        provenance: resolved.provenance,
        lifecycle_authority: resolved.lifecycle_authority,
        launch_timings: None,
        original_root_kind: if graph_ref.starts_with("tool:") {
            "tool"
        } else {
            "graph"
        },
        pre_minted_thread_id,
        usage_subject: None,
        usage_subject_asserted_by: None,
        previous_thread_id: None,
        root_admission: None,
        root_dispatch_evidence: None,
        parent_execution_context: None,
        effect_authority: None,
    };
    let result = ryeos_executor::dispatch::dispatch_with_handler_context(
        graph_ref, context, &request, &exec_ctx, &state,
    )
    .await
    .map_err(|error| anyhow::anyhow!("{graph_ref} dispatch failed: {error}"));
    // The context owns the staged snapshot roots until dispatch has made its
    // authoritative birth/result rows visible. It must not be dropped earlier.
    drop(project_ctx.take_captured_generation());
    drop(workspace);
    result
}

fn dispatch_result(envelope: &Value) -> anyhow::Result<&Value> {
    let terminal = envelope
        .get("result")
        .context("release execution omitted its terminal result envelope")?;
    anyhow::ensure!(
        terminal.get("outcome_code").and_then(Value::as_str) == Some("success"),
        "release execution did not complete successfully"
    );
    terminal
        .get("result")
        .filter(|value| !value.is_null())
        .context("release execution omitted its successful result")
}

fn accept_dispatch_products(
    envelope: &Value,
    state: &AppState,
) -> anyhow::Result<(
    String,
    ryeos_state::external_content::products::accepted_result::ProductBuildAcceptedResult,
)> {
    let thread_id = envelope
        .pointer("/thread/thread_id")
        .and_then(Value::as_str)
        .context("release producer omitted its terminal thread identity")?;
    let authority = state.state_store.pinned_state_authority()?;
    let guard = authority.acquire_shared_guard()?;
    let capsule = state
        .state_store
        .admitted_launch_capsule(thread_id)?
        .context("release producer has no admitted launch capsule")?;
    let accepted = ryeos_app::operator_external_content::product_build::accept_terminal(
        state, &capsule, thread_id, &guard,
    )?;
    ryeos_app::operator_external_content::product_build::verify_current(
        state,
        &guard,
        &accepted.to_value()?,
        &capsule,
    )?;
    let hash = authority.cas_store()?.store_object(&accepted.to_value()?)?;
    Ok((hash, accepted))
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
        let publication =
            ryeos_app::operator_external_content::product_qualification::qualify(
                Arc::clone(&state),
                context,
                ryeos_app::operator_external_content::product_qualification::ProductQualificationRequest {
                    witness_hash: request.signed_product_witness,
                    witness_source: ryeos_state::external_content::products::transfer::ProductWitnessSource::LocalCapture {},
                    relationship_name: "signed_native_bundle_to_release_qualification".to_owned(),
                    verifier_chain_root_id: verifier_chain_root_id.clone(),
                    verifier_thread_id: verifier_chain_root_id,
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
    let limits = state
        .node_policy
        .require::<ryeos_app::node_policy::sections::object_closure::NodeObjectClosurePolicy>()?
        .closure_limits()?;
    Ok(ContextualReleaseProof {
        state,
        context,
        authority,
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
pub const GENERATION_CAPTURE: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:bundle-release/generation-capture",
    endpoint: "bundle_release.generation_capture",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.bundle-release/generation-capture"],
    handler: generation_capture_handler,
};
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

#[cfg(test)]
mod tests {
    use super::*;

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
        let workspace =
            materialize_release_execution_project(source.path(), &source_hash, signed, None)
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
            materialize_release_execution_project(source.path(), &"f".repeat(64), signed, None,)
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
