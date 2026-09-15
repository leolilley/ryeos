//! Operator-owned capture and exact lookup of named admitted build products.
//!
//! Calls carry coordinates only. The terminal capsule owns recipe identity,
//! declarations and output selection. Product testimony is not a consumer grant.
//! Local and configured remote operators may capture only their own admitted
//! producer results; this never grants ambient import or another owner's bytes.

use super::*;
use ryeos_state::external_content::products::admission::{
    admitted_product_producer, admitted_product_recipe,
};
use ryeos_state::external_content::products::publication::{
    ProductCaptureCoordinate, ProductWitnessLookup, VerifiedProductWitness,
    lookup_product_witness_guarded, publish_product_witness,
};
use ryeos_state::external_content::products::{
    ProductCaptureEvidence, ProductDeclaration, ProductShape, ProductSource, ProductStorage,
};
use ryeos_state::external_content::retained_workspace_output::RetainedWorkspaceOutputContent;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProductRequest {
    pub chain_root_id: String,
    pub thread_id: String,
    pub recipe_binding: String,
    pub product_name: String,
}

impl ProductRequest {
    fn coordinate(&self, owner_principal: &str) -> anyhow::Result<ProductCaptureCoordinate> {
        ryeos_runtime::validate_runtime_thread_id(&self.chain_root_id)
            .map_err(anyhow::Error::msg)?;
        ryeos_runtime::validate_runtime_thread_id(&self.thread_id).map_err(anyhow::Error::msg)?;
        let coordinate = ProductCaptureCoordinate {
            owner_principal: owner_principal.to_owned(),
            chain_root_id: self.chain_root_id.clone(),
            thread_id: self.thread_id.clone(),
            recipe_binding: self.recipe_binding.clone(),
            product_name: self.product_name.clone(),
        };
        coordinate.validate()?;
        Ok(coordinate)
    }
}

#[derive(Debug, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ProductResponse {
    Captured {
        coordinate_id: String,
        witness_hash: String,
        evidence: ProductCaptureEvidence,
        idempotent: bool,
    },
    Missing {
        coordinate_id: String,
    },
    /// Absence is established from the still-retained terminal snapshot. It is
    /// not a product witness or a promise to retain an absent output forever.
    AbsentOptional {
        coordinate_id: String,
    },
}

fn found_response(
    coordinate_id: String,
    witness: VerifiedProductWitness,
    idempotent: bool,
) -> ProductResponse {
    ProductResponse::Captured {
        coordinate_id,
        witness_hash: witness.attestation_hash,
        evidence: witness.evidence,
        idempotent,
    }
}

pub async fn get(
    state: Arc<AppState>,
    context: HandlerContext,
    request: ProductRequest,
) -> anyhow::Result<ProductResponse> {
    crate::operator_authority::require_admitted_operator(&state, &context)?;
    let coordinate = request.coordinate(&context.fingerprint)?;
    tokio::task::spawn_blocking(move || {
        let authority = state.state_store.pinned_state_authority()?;
        let guard = authority.acquire_shared_guard()?;
        let limits = state
            .node_policy
            .require::<NodeObjectClosurePolicy>()?
            .closure_limits()?;
        let coordinate_id = coordinate.coordinate_id()?;
        match lookup_product_witness_guarded(
            &authority,
            &coordinate,
            state.identity.verifying_key(),
            limits,
            &guard,
        )? {
            ProductWitnessLookup::Missing => Ok(ProductResponse::Missing { coordinate_id }),
            ProductWitnessLookup::Found(witness) => {
                Ok(found_response(coordinate_id, witness, true))
            }
        }
    })
    .await
    .context("product lookup task stopped")?
}

pub async fn capture(
    state: Arc<AppState>,
    context: HandlerContext,
    request: ProductRequest,
) -> anyhow::Result<ProductResponse> {
    crate::operator_authority::require_admitted_operator(&state, &context)?;
    request.coordinate(&context.fingerprint)?;
    tokio::task::spawn_blocking(move || capture_blocking(state, context, request))
        .await
        .context("product capture task stopped")?
}

// Request-local verified source. Only this owner constructs it from durable
// terminal, capsule, recipe and exact machine-lineage facts. It never outlives
// the batch's CAS guard and is not a caller-authored admission token.
#[derive(Clone)]
struct ProductCaptureSource {
    owner: String,
    chain_root_id: String,
    thread_id: String,
    recipe_binding: String,
    capsule_hash: String,
    snapshot_hash: String,
    result_generation: ryeos_state::objects::WorkspaceGenerationPair,
    producer: ryeos_state::external_content::products::ProductProducerAdmission,
    root_producer: ryeos_state::external_content::products::ProductProducerAdmission,
    recipe: ryeos_state::external_content::products::admission::AdmittedProductRecipeBinding,
}

pub(super) fn capture_blocking(
    state: Arc<AppState>,
    context: HandlerContext,
    request: ProductRequest,
) -> anyhow::Result<ProductResponse> {
    let authority = state.state_store.pinned_state_authority()?;
    let guard = authority.acquire_shared_guard()?;
    let limits = state
        .node_policy
        .require::<NodeObjectClosurePolicy>()?
        .closure_limits()?;
    capture_guarded(
        state, context, request, &authority, &guard, limits, &mut None,
    )
}

/// Capture a finite batch from one exact terminal under the caller's existing
/// publication guard. Product lookup/import/policy/publication checks are still
/// per product; only immutable source verification is reused.
pub(super) fn capture_batch_blocking(
    state: Arc<AppState>,
    context: HandlerContext,
    requests: Vec<ProductRequest>,
    guard: &ryeos_state::CasMutationGuard,
) -> anyhow::Result<Vec<ProductResponse>> {
    crate::operator_authority::require_admitted_operator(&state, &context)?;
    validate_capture_batch_requests(&requests, &context.fingerprint)?;
    let authority = state.state_store.pinned_state_authority()?;
    authority.ensure_guard(guard)?;
    let limits = state
        .node_policy
        .require::<NodeObjectClosurePolicy>()?
        .closure_limits()?;
    let mut source = None;
    requests
        .into_iter()
        .map(|request| {
            capture_guarded(
                state.clone(),
                context.clone(),
                request,
                &authority,
                guard,
                limits,
                &mut source,
            )
        })
        .collect()
}

fn validate_capture_batch_requests(requests: &[ProductRequest], owner: &str) -> anyhow::Result<()> {
    if requests.is_empty() || requests.len() > ryeos_state::external_content::products::MAX_PRODUCTS
    {
        bail!("product capture batch has an invalid declaration count");
    }
    let first = &requests[0];
    let mut names = std::collections::BTreeSet::new();
    for request in requests {
        request.coordinate(owner)?;
        if request.chain_root_id != first.chain_root_id
            || request.thread_id != first.thread_id
            || request.recipe_binding != first.recipe_binding
            || !names.insert(&request.product_name)
        {
            bail!(
                "product capture batch requires distinct products from one exact terminal recipe"
            );
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn capture_guarded(
    state: Arc<AppState>,
    context: HandlerContext,
    request: ProductRequest,
    authority: &ryeos_state::PinnedStateAuthority,
    guard: &ryeos_state::CasMutationGuard,
    limits: ryeos_state::object_closure::ObjectClosureLimits,
    source: &mut Option<ProductCaptureSource>,
) -> anyhow::Result<ProductResponse> {
    authority.ensure_guard(guard)?;
    let operator = crate::operator_authority::require_admitted_operator(&state, &context)?;
    let coordinate = request.coordinate(&context.fingerprint)?;
    let coordinate_id = coordinate.coordinate_id()?;
    // Keep lookup before source verification: immutable published decisions
    // remain available after their historical producer objects are collected.
    if let ProductWitnessLookup::Found(witness) = lookup_product_witness_guarded(
        authority,
        &coordinate,
        state.identity.verifying_key(),
        limits,
        guard,
    )? {
        return Ok(found_response(coordinate_id, witness, true));
    }
    if source.is_none() {
        *source = Some(load_capture_source(
            &state, &context, &request, authority, guard, limits,
        )?);
    }
    let source = source
        .as_ref()
        .context("verified product source was not prepared")?;
    if source.owner != context.fingerprint
        || source.chain_root_id != request.chain_root_id
        || source.thread_id != request.thread_id
        || source.recipe_binding != request.recipe_binding
    {
        bail!("product capture batch crossed its exact verified source coordinate");
    }
    let ProductCaptureSource {
        capsule_hash,
        snapshot_hash,
        result_generation,
        producer,
        root_producer,
        recipe,
        ..
    } = source.clone();
    let product = recipe.declarations.select(&request.product_name)?.clone();
    let policy = state.node_policy.require::<crate::node_policy::sections::external_content::ExternalContentImportPolicyRecord>()?;
    let capture_policy_digest = ryeos_state::objects::canonical_value_digest(&serde_json::json!({
        "limits": policy.limits,
        "capture_floor_rules": ryeos_state::project_sync::durable_content_capture_floor_rules(),
        "configured_ignore_patterns": state.ignore_matcher.canonical_patterns(),
    }))?;
    let selected_output_capture_hash =
        selected_workspace_output_capture_hash(&product, &result_generation)?;
    let (import, workspace_output_capture_hash, producer_partition_identity) = match &product.source
    {
        ProductSource::RetainedProject {} => {
            let import_request = RetainedResultImportRequest {
                chain_root_id: request.chain_root_id.clone(),
                thread_id: request.thread_id.clone(),
                result_project_snapshot_hash: snapshot_hash.clone(),
                path: product.path.clone(),
                shape: match product.shape {
                    ProductShape::File => ImportShape::File,
                    ProductShape::Tree => ImportShape::Tree,
                },
                storage: match product.storage {
                    ProductStorage::Content => ImportStorage::Content,
                    ProductStorage::LargeContent => ImportStorage::LargeContent,
                },
                maximum_bytes: product
                    .bounds
                    .maximum_total_bytes
                    .min(policy.limits.max_total_bytes),
                expected_file_sha256: None,
            };
            (
                super::retained_result::import_product(
                    state.clone(),
                    context,
                    import_request,
                    &product,
                )?,
                None,
                None,
            )
        }
        ProductSource::WorkspaceOutput { .. } => {
            let capture_hash = selected_output_capture_hash
                .context("workspace-output product has no terminal output capture")?;
            let (import, partition_identity) = import_workspace_output_product(
                &state,
                &authority,
                &guard,
                limits,
                &operator,
                &coordinate,
                capture_hash,
                &recipe,
                &product,
                policy,
            )?;
            (
                import,
                Some(capture_hash.to_owned()),
                Some(partition_identity),
            )
        }
    };
    let Some(import) = import else {
        return Ok(ProductResponse::AbsentOptional { coordinate_id });
    };
    let evidence = ProductCaptureEvidence {
        schema: ryeos_state::external_content::products::PRODUCT_CAPTURE_EVIDENCE_SCHEMA,
        owner_principal: coordinate.owner_principal.clone(),
        chain_root_id: request.chain_root_id,
        thread_id: request.thread_id,
        admitted_launch_capsule_hash: capsule_hash,
        producer,
        root_producer,
        result_project_snapshot_hash: snapshot_hash,
        workspace_output_capture_hash,
        producer_partition_identity,
        recipe_binding: request.recipe_binding,
        recipe_ref: recipe.recipe_ref,
        recipe_raw_content_digest: recipe.recipe_raw_content_digest,
        declarations_hash: recipe.declarations_hash,
        declarations: recipe.declarations,
        relationships: recipe.relationships,
        declaration: product,
        capture_policy_digest,
        manifest_hash: import.manifest_hash,
        manifest_kind: import.manifest_kind,
        entry_count: import.entry_count,
        total_bytes: import.total_bytes,
    };
    let signer = crate::state_store::NodeIdentitySigner::from_identity(&state.identity);
    let attestation = evidence.sign_attestation(&signer, lillux::time::iso8601_now())?;
    let _permit = state
        .write_barrier
        .acquire_with_timeout(crate::write_barrier::ONLINE_WRITE_PERMIT_TIMEOUT)
        .map_err(|error| anyhow::anyhow!("cannot acquire product publication permit: {error}"))?;
    let published = publish_product_witness(
        &authority,
        &coordinate,
        &attestation,
        limits,
        &signer,
        &guard,
    )?;
    // Publication owns the witness/manifest closure. Settle only this capture's
    // upload; later consumer binding uses a fresh stage, not this capability.
    // The signed head is the committed answer. A staging cleanup failure must
    // not relabel that durable success as a failed capture. Retry reads the
    // witness, and the existing upload age policy retires abandoned staging.
    let settled = (|| -> anyhow::Result<()> {
        let mut stage = authority
            .require_recovery()?
            .open_durable_cas_upload_admitted(&guard, &import.staging_id, &operator)?;
        stage.ensure_publication_contract(
            &ryeos_state::DurableCasPublicationKey::external_content_import(
                &import.request_digest,
            )?,
            None,
        )?;
        stage.protect_cas_closure(
            &guard,
            [published.witness.attestation_hash.as_str()],
            std::iter::empty(),
        )?;
        stage.finish_admitted(&guard, &published.witness.attestation_hash)?;
        Ok(())
    })();
    if let Err(error) = settled {
        tracing::warn!(%error, staging_id = %import.staging_id,
            witness_hash = %published.witness.attestation_hash,
            "product witness published; import stage cleanup remains pending");
    }
    Ok(found_response(
        coordinate_id,
        published.witness,
        published.reused_existing,
    ))
}

fn load_capture_source(
    state: &AppState,
    context: &HandlerContext,
    request: &ProductRequest,
    authority: &ryeos_state::PinnedStateAuthority,
    guard: &ryeos_state::CasMutationGuard,
    limits: ryeos_state::object_closure::ObjectClosureLimits,
) -> anyhow::Result<ProductCaptureSource> {
    authority.ensure_guard(guard)?;
    let (thread, _, _) = state
        .state_store
        .get_authoritative_thread_snapshot_with_last_event(
            &request.chain_root_id,
            &request.thread_id,
        )?
        .context("product producer terminal does not exist")?;
    let root = state
        .state_store
        .get_authoritative_root_thread_snapshot(&request.chain_root_id)?
        .context("product producer root does not exist")?;
    let snapshot_hash = thread
        .result_project_snapshot_hash
        .clone()
        .context("producer has no retained result snapshot")?;
    let capsule_hash = thread
        .admitted_launch_capsule_hash
        .clone()
        .context("producer has no admitted capsule")?;
    // Establish ownership and terminal success before loading producer payloads.
    super::retained_result::authorize_terminal_result(
        &root,
        &thread,
        &request.chain_root_id,
        &request.thread_id,
        &snapshot_hash,
        &context.fingerprint,
    )?;
    let cas = authority.cas_store()?;
    let result_generation = state
        .state_store
        .authoritative_result_generation(&request.thread_id)?
        .context("producer has no authoritative terminal result generation")?;
    if result_generation.snapshot_hash != snapshot_hash {
        bail!("producer terminal result generation changed during product capture");
    }
    let capsule = ryeos_state::objects::AdmittedLaunchCapsule::from_current_value(
        ryeos_state::object_closure::load_exact_cas_object_with_cas(
            &cas,
            &capsule_hash,
            limits.max_object_bytes,
        )?,
    )?;
    capsule.verify_retained_execution_realization(
        &cas,
        &authority.large_object_store()?,
        authority.trust_store(),
    )?;
    let sealed = crate::thread_lifecycle::SealedRootExecutionRequest::decode_from_admitted_capsule(
        &capsule,
    )?;
    let producer = admitted_product_producer(&capsule)?;
    if producer.canonical_ref != sealed.item_ref()
        || producer.effective_definition_digest != sealed.effective_definition_digest().as_str()
        || producer.canonical_ref != thread.item_ref
        || capsule.project_authority != thread.project_authority
        || thread.base_project_snapshot_hash.as_deref()
            != Some(producer.producer_project_snapshot_hash.as_str())
        || producer.admitted_parameters_digest != sealed.admitted_parameters_digest()?
    {
        bail!("product producer capsule contradicts its terminal project authority");
    }
    let recipe = admitted_product_recipe(&capsule, &request.recipe_binding)?;
    let (root_producer, _) = super::product_build::verified_root_producer(
        &state,
        &request.chain_root_id,
        &request.thread_id,
        &request.recipe_binding,
        &guard,
    )?;

    Ok(ProductCaptureSource {
        owner: context.fingerprint.clone(),
        chain_root_id: request.chain_root_id.clone(),
        thread_id: request.thread_id.clone(),
        recipe_binding: request.recipe_binding.clone(),
        capsule_hash,
        snapshot_hash,
        result_generation,
        producer,
        root_producer,
        recipe,
    })
}

/// Select the source coordinate exactly as admitted. A retained-project
/// declaration never falls back to an output capture, and a workspace-output
/// declaration cannot fall back to the retained project snapshot.
fn selected_workspace_output_capture_hash<'a>(
    product: &ProductDeclaration,
    result_generation: &'a ryeos_state::objects::WorkspaceGenerationPair,
) -> anyhow::Result<Option<&'a str>> {
    match &product.source {
        ProductSource::RetainedProject {} => Ok(None),
        ProductSource::WorkspaceOutput { .. } => result_generation
            .output_capture_hash
            .as_deref()
            .map(Some)
            .context("workspace-output product has no terminal output capture"),
    }
}

#[allow(clippy::too_many_arguments)]
fn import_workspace_output_product(
    state: &AppState,
    authority: &ryeos_state::PinnedStateAuthority,
    guard: &ryeos_state::CasMutationGuard,
    limits: ryeos_state::object_closure::ObjectClosureLimits,
    operator: &str,
    coordinate: &ProductCaptureCoordinate,
    capture_hash: &str,
    recipe: &ryeos_state::external_content::products::admission::AdmittedProductRecipeBinding,
    product: &ryeos_state::external_content::products::ProductDeclaration,
    policy: &crate::node_policy::sections::external_content::ExternalContentImportPolicyRecord,
) -> anyhow::Result<(Option<ImportResponse>, String)> {
    authority.ensure_guard(guard)?;
    let cas = authority.cas_store()?;
    let capture_value = ryeos_state::object_closure::load_exact_cas_object_with_cas(
        &cas,
        capture_hash,
        (ryeos_state::objects::MAX_WORKSPACE_OUTPUT_CAPTURE_BYTES as u64)
            .min(limits.max_object_bytes),
    )?;
    let capture = ryeos_state::objects::WorkspaceOutputCapture::from_value(&capture_value)?;
    verify_workspace_output_recipe(&capture.partition, recipe)?;
    let snapshot_policy = ryeos_state::project_materialization::load_project_policy_bounded(
        &cas,
        &capture.partition.project_snapshot_policy_hash,
    )?
    .context("workspace-output capture policy is unavailable")?;
    let node_bounds = ryeos_state::LargeContentCaptureBounds {
        max_depth: policy.limits.max_depth,
        max_entries: policy.limits.max_entries,
        max_file_bytes: policy.limits.max_file_bytes,
        max_total_bytes: policy.limits.max_total_bytes,
    };
    let Some(selected) = RetainedWorkspaceOutputContent::select(
        &cas,
        &capture,
        product,
        &snapshot_policy,
        state.ignore_matcher.as_ref(),
        &node_bounds,
        limits,
    )?
    else {
        return Ok((None, capture.partition.partition_identity));
    };
    let manifest_value = selected.to_value()?;
    let expected_manifest_hash = selected.manifest_hash()?;
    let manifest_kind = selected.manifest_kind().to_owned();
    let entry_count = selected.entry_count();
    let total_bytes = selected.total_bytes();
    require_import_store_capacity(
        "external-content CAS",
        cas.filesystem_capacity()?,
        policy.limits.minimum_free_bytes,
        ryeos_state::objects::MAX_LARGE_CONTENT_MANIFEST_BYTES as u64,
        entry_count,
    )?;
    let digest = lillux::sha256_hex(
        lillux::canonical_json(&serde_json::json!({
            "source": "retained_workspace_output_product",
            "coordinate": coordinate,
            "workspace_output_capture_hash": capture_hash,
            "product_declaration": product,
            "operator": operator,
            "node": state.identity.fingerprint(),
            "limits": policy.limits,
            "closure_limits": {
                "max_objects": limits.max_objects,
                "max_blobs": limits.max_blobs,
                "max_object_bytes": limits.max_object_bytes,
                "max_total_object_bytes": limits.max_total_object_bytes,
                "max_blob_bytes": limits.max_blob_bytes,
                "max_total_blob_bytes": limits.max_total_blob_bytes,
                "max_links_per_object": limits.max_links_per_object,
            },
            "capture_floor_rules": ryeos_state::project_sync::durable_content_capture_floor_rules(),
            "configured_ignore_patterns": state.ignore_matcher.canonical_patterns(),
        }))?
        .as_bytes(),
    );
    let key = ryeos_state::DurableCasPublicationKey::external_content_import(&digest)?;
    let _permit = state
        .write_barrier
        .acquire_with_timeout(crate::write_barrier::ONLINE_WRITE_PERMIT_TIMEOUT)
        .map_err(|error| {
            anyhow::anyhow!("cannot acquire workspace-output publication permit: {error}")
        })?;
    let mut stage = authority
        .require_recovery()?
        .begin_durable_cas_upload_admitted(
            guard,
            operator,
            "external-content-import",
            &key,
            None,
        )?;
    let manifest_hash = cas.store_object(&manifest_value)?;
    if manifest_hash != expected_manifest_hash {
        bail!("workspace-output selected manifest changed during storage");
    }
    let closure = ryeos_state::object_closure::collect_object_closure_with_cas_and_limits(
        &cas,
        [manifest_hash.clone()],
        limits,
    )?;
    if !closure.is_complete() {
        bail!("workspace-output selected product closure is incomplete");
    }
    for hash in &closure.large_object_hashes {
        stage.protect_large_object_hash(guard, hash)?;
    }
    stage.protect_cas_closure(guard, [manifest_hash.as_str()], std::iter::empty())?;
    Ok((
        Some(ImportResponse {
            staging_id: stage.staging_id().to_owned(),
            request_digest: digest,
            manifest_hash,
            manifest_kind,
            entry_count,
            total_bytes,
        }),
        capture.partition.partition_identity,
    ))
}

fn verify_workspace_output_recipe(
    partition: &ryeos_state::objects::WorkspaceOutputPartition,
    recipe: &ryeos_state::external_content::products::admission::AdmittedProductRecipeBinding,
) -> anyhow::Result<()> {
    if partition.recipe_binding != recipe.binding_name
        || partition.recipe_ref != recipe.recipe_ref
        || partition.recipe_raw_content_digest != recipe.recipe_raw_content_digest
        || partition.declarations_hash != recipe.declarations_hash
    {
        bail!("workspace-output capture contradicts the admitted product recipe");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn declaration(source: ProductSource) -> ProductDeclaration {
        ProductDeclaration {
            name: "runtime".into(),
            source,
            path: "products/runtime".into(),
            shape: ProductShape::Tree,
            storage: ProductStorage::Content,
            required: true,
            bounds: ryeos_state::external_content::products::ProductBounds {
                maximum_entries: 4,
                maximum_depth: 2,
                maximum_file_bytes: 1024,
                maximum_total_bytes: 4096,
            },
            expected_manifest_hash: None,
        }
    }

    fn recipe_and_partition() -> (
        ryeos_state::external_content::products::admission::AdmittedProductRecipeBinding,
        ryeos_state::objects::WorkspaceOutputPartition,
    ) {
        let declaration = declaration(ProductSource::WorkspaceOutput {
            root: "distribution".into(),
        });
        let declarations = ryeos_state::external_content::products::ProductDeclarations {
            schema: ryeos_state::external_content::products::PRODUCT_DECLARATIONS_SCHEMA.into(),
            output_roots: vec![ryeos_state::objects::WorkspaceOutputRootDeclaration {
                name: "distribution".into(),
                path: "products".into(),
                storage: ProductStorage::Content,
                bounds: declaration.bounds.clone(),
            }],
            products: vec![declaration.clone()],
        };
        let declarations_hash = declarations.content_hash().unwrap();
        let recipe = ryeos_state::external_content::products::admission::AdmittedProductRecipeBinding {
            schema: ryeos_state::external_content::products::admission::PRODUCT_RECIPE_BINDING_SCHEMA.into(),
            binding_name: "product_recipe".into(),
            recipe_ref: "config:test/products".into(),
            recipe_raw_content_digest: "a".repeat(64),
            declarations,
            declarations_hash: declarations_hash.clone(),
            relationships: ryeos_state::external_content::products::composition::ProductRelationships::empty(),
        };
        let bounds = declaration.bounds.clone();
        let partition = ryeos_state::objects::WorkspaceOutputPartition {
            schema: ryeos_state::objects::WORKSPACE_OUTPUT_PARTITION_SCHEMA.into(),
            recipe_binding: recipe.binding_name.clone(),
            recipe_ref: recipe.recipe_ref.clone(),
            recipe_raw_content_digest: recipe.recipe_raw_content_digest.clone(),
            declarations_hash,
            project_snapshot_policy_hash: "b".repeat(64),
            roots: vec![ryeos_state::objects::WorkspaceOutputRoot {
                name: "distribution".into(),
                path: "products".into(),
                storage: ProductStorage::Content,
                declared_bounds: bounds.clone(),
                effective_bounds: bounds,
            }],
            products: vec![declaration],
            partition_identity: "c".repeat(64),
            capture_policy_digest: "d".repeat(64),
        };
        (recipe, partition)
    }

    fn request_value() -> serde_json::Value {
        serde_json::json!({
            "chain_root_id": "T-00000000-0000-0000-0000-000000000001",
            "thread_id": "T-00000000-0000-0000-0000-000000000002",
            "recipe_binding": "product_recipe",
            "product_name": "runtime",
        })
    }

    #[test]
    fn capture_batch_is_bounded_to_distinct_products_of_one_exact_terminal_recipe() {
        let owner = format!("fp:{}", "a".repeat(64));
        let first: ProductRequest = serde_json::from_value(request_value()).unwrap();
        let mut second = first.clone();
        second.product_name = "headers".into();
        validate_capture_batch_requests(&[first.clone(), second.clone()], &owner).unwrap();
        assert!(validate_capture_batch_requests(&[], &owner).is_err());
        assert!(validate_capture_batch_requests(&[first.clone(), first.clone()], &owner).is_err());
        for field in ["chain_root_id", "thread_id", "recipe_binding"] {
            let mut value = serde_json::to_value(&second).unwrap();
            value[field] = if field == "recipe_binding" {
                "other_recipe".into()
            } else {
                "T-00000000-0000-0000-0000-000000000003".into()
            };
            let changed = serde_json::from_value(value).unwrap();
            assert!(validate_capture_batch_requests(&[first.clone(), changed], &owner).is_err());
        }
        let oversized = (0..=ryeos_state::external_content::products::MAX_PRODUCTS)
            .map(|index| {
                let mut request = first.clone();
                request.product_name = format!("product_{index}");
                request
            })
            .collect::<Vec<_>>();
        assert!(validate_capture_batch_requests(&oversized, &owner).is_err());
        assert!(validate_capture_batch_requests(&[first], "untrusted-owner").is_err());
    }

    #[test]
    fn capture_accepts_only_coordinates_not_caller_product_authority() {
        for field in [
            "path",
            "manifest_hash",
            "declarations",
            "owner_principal",
            "result_project_snapshot_hash",
        ] {
            let mut value = request_value();
            value[field] = serde_json::json!("caller controlled");
            assert!(
                serde_json::from_value::<ProductRequest>(value).is_err(),
                "{field}"
            );
        }
        let request: ProductRequest = serde_json::from_value(request_value()).unwrap();
        request
            .coordinate(&format!("fp:{}", "a".repeat(64)))
            .unwrap();
    }

    #[test]
    fn exact_owner_and_product_change_the_capture_coordinate() {
        let mut request: ProductRequest = serde_json::from_value(request_value()).unwrap();
        let owner = format!("fp:{}", "a".repeat(64));
        let original = request.coordinate(&owner).unwrap().coordinate_id().unwrap();
        let other_owner = request
            .coordinate(&format!("fp:{}", "b".repeat(64)))
            .unwrap()
            .coordinate_id()
            .unwrap();
        assert_ne!(original, other_owner);
        request.product_name = "distribution".into();
        assert_ne!(
            original,
            request.coordinate(&owner).unwrap().coordinate_id().unwrap()
        );
        request.product_name = "../runtime".into();
        assert!(request.coordinate(&owner).is_err());
    }

    #[test]
    fn product_commands_bind_exact_coordinates_through_registered_contracts() {
        let cases = [
            (
                include_str!(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/../../../bundles/core/.ai/node/commands/external-content-capture-product.yaml"
                )),
                include_str!(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/../../../bundles/core/.ai/services/external-content/capture-product.yaml"
                )),
            ),
            (
                include_str!(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/../../../bundles/core/.ai/node/commands/external-content-product.yaml"
                )),
                include_str!(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/../../../bundles/core/.ai/services/external-content/product.yaml"
                )),
            ),
        ];
        for (command, service) in cases {
            let command: ryeos_runtime::command::CommandDef =
                serde_yaml::from_str(command).unwrap();
            let service: serde_json::Value = serde_yaml::from_str(service).unwrap();
            let contract =
                ryeos_runtime::command::InvocationInputContract::from_lightweight_schema_value(
                    &service["schema"],
                )
                .unwrap()
                .unwrap();
            let value = request_value();
            let argv = vec![
                value["chain_root_id"].as_str().unwrap().to_owned(),
                value["thread_id"].as_str().unwrap().to_owned(),
                "runtime".to_owned(),
            ];
            let bound = ryeos_runtime::arg_binder::bind_argv_with_command_and_contract(
                &argv,
                Some(&command),
                Some(&contract),
            )
            .unwrap();
            assert_eq!(bound, value);
            let parsed: ProductRequest = serde_json::from_value(bound).unwrap();
            parsed
                .coordinate(&format!("fp:{}", "a".repeat(64)))
                .unwrap();
        }
    }

    #[test]
    fn product_import_command_preserves_distinct_import_source() {
        let command: ryeos_runtime::command::CommandDef =
            serde_yaml::from_str(include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../../bundles/core/.ai/node/commands/external-content-import-product.yaml"
            )))
            .unwrap();
        let service: serde_json::Value = serde_yaml::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../bundles/core/.ai/services/external-content/import.yaml"
        )))
        .unwrap();
        let contract =
            ryeos_runtime::command::InvocationInputContract::from_lightweight_schema_value(
                &service["schema"],
            )
            .unwrap()
            .unwrap();
        let bound = ryeos_runtime::arg_binder::bind_argv_with_command_and_contract(
            &[
                "a".repeat(64),
                r#"{"kind":"local_capture"}"#.to_owned(),
                "4096".to_owned(),
            ],
            Some(&command),
            Some(&contract),
        )
        .unwrap();
        assert_eq!(
            bound,
            serde_json::json!({
                "source": "retained_product", "witness_hash": "a".repeat(64),
                "witness_source": {"kind":"local_capture"}, "maximum_bytes": 4096,
            })
        );
        assert!(matches!(
            serde_json::from_value::<ImportRequest>(bound).unwrap(),
            ImportRequest::RetainedProduct(_)
        ));
    }

    #[test]
    fn signed_product_source_selects_exactly_one_terminal_generation_member() {
        let capture_hash = "b".repeat(64);
        let with_output = ryeos_state::objects::WorkspaceGenerationPair {
            snapshot_hash: "a".repeat(64),
            output_capture_hash: Some(capture_hash.clone()),
        };
        assert_eq!(
            selected_workspace_output_capture_hash(
                &declaration(ProductSource::RetainedProject {}),
                &with_output,
            )
            .unwrap(),
            None,
            "retained-project capture must not consume the output capture"
        );
        assert_eq!(
            selected_workspace_output_capture_hash(
                &declaration(ProductSource::WorkspaceOutput {
                    root: "distribution".into(),
                }),
                &with_output,
            )
            .unwrap(),
            Some(capture_hash.as_str())
        );

        let without_output = ryeos_state::objects::WorkspaceGenerationPair {
            snapshot_hash: "a".repeat(64),
            output_capture_hash: None,
        };
        assert!(
            selected_workspace_output_capture_hash(
                &declaration(ProductSource::WorkspaceOutput {
                    root: "distribution".into(),
                }),
                &without_output,
            )
            .unwrap_err()
            .to_string()
            .contains("no terminal output capture")
        );
    }

    #[test]
    fn workspace_output_capture_requires_the_exact_admitted_recipe_partition() {
        let (recipe, partition) = recipe_and_partition();
        verify_workspace_output_recipe(&partition, &recipe).unwrap();
        for change in 0..4 {
            let mut changed = partition.clone();
            match change {
                0 => changed.recipe_binding = "other_recipe".into(),
                1 => changed.recipe_ref = "config:test/other".into(),
                2 => changed.recipe_raw_content_digest = "e".repeat(64),
                _ => changed.declarations_hash = "f".repeat(64),
            }
            assert!(verify_workspace_output_recipe(&changed, &recipe).is_err());
        }
    }
}
