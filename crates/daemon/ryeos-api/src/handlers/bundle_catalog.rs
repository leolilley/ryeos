//! Native bundle-catalog transport, resolution, and recovery surfaces.

use std::sync::Arc;

use anyhow::{Context as _, Result, bail};
use base64::Engine as _;
use ryeos_app::{
    bundle_publication::catalog::{
        BUNDLE_CATALOG_HEAD_NAMESPACE, CatalogPublicationRequest, LocalCatalogClosureStageRequest,
        inspect_catalog, publish_catalog, stage_local_catalog_closure,
    },
    handler_context::HandlerContext,
    node_policy::sections::{
        bundle_publication::BundlePublicationPolicy, object_closure::NodeObjectClosurePolicy,
    },
    state::AppState,
};
use ryeos_executor::executor::ServiceAvailability;
use serde_json::Value;

use crate::registry::ServiceDescriptor;

const MAX_UPLOAD_ENTRIES: usize = 4096;
const MAX_INLINE_BLOB_BYTES: usize = 16 * 1024 * 1024;
const MAX_BLOB_BYTES: u64 = 512 * 1024 * 1024;
const MAX_BLOB_CHUNK_BYTES: usize = 512 * 1024;

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UploadBlobChunk {
    pub hash: String,
    pub total_size: u64,
    pub offset: u64,
    pub data_base64: String,
}

#[derive(serde::Deserialize)]
#[serde(transparent)]
pub struct ExpectedCatalogHead(Option<String>);

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UploadRequest {
    #[serde(default)]
    pub upload_session_id: Option<String>,
    pub catalog_namespace: String,
    pub expected_catalog_head: ExpectedCatalogHead,
    #[serde(default)]
    pub objects: Vec<Value>,
    #[serde(default)]
    pub blobs_base64: Vec<String>,
    #[serde(default)]
    pub blob_chunks: Vec<UploadBlobChunk>,
}

pub async fn upload(
    req: UploadRequest,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value> {
    ctx.require_verified()
        .map_err(|error| anyhow::anyhow!(error))?;
    if req
        .objects
        .len()
        .saturating_add(req.blobs_base64.len())
        .saturating_add(req.blob_chunks.len())
        > MAX_UPLOAD_ENTRIES
    {
        bail!("catalog upload batch exceeds the entry bound");
    }
    let policy = state.node_policy.require::<BundlePublicationPolicy>()?;
    let catalog = policy.require_catalog(&req.catalog_namespace)?;
    catalog.require_uploader(&ctx.fingerprint)?;
    let policy_digest = policy.section_digest()?;
    let publication_key = ryeos_state::DurableCasPublicationKey::bundle_catalog(
        &catalog.publisher_fingerprint,
        &req.catalog_namespace,
        req.expected_catalog_head.0.as_deref(),
        &policy_digest,
        state.node_policy.generation_digest(),
    )?;
    let _permit = state
        .write_barrier
        .acquire_with_timeout(ryeos_app::write_barrier::ONLINE_WRITE_PERMIT_TIMEOUT)
        .map_err(|error| anyhow::anyhow!("cannot acquire catalog upload permit: {error}"))?;
    let authority = state.state_store.pinned_state_authority()?;
    let guard = authority.acquire_shared_guard()?;
    let cas = authority.cas_store()?;
    let recovery = authority.require_recovery()?;
    let mut stage = match req.upload_session_id.as_deref() {
        Some(id) => {
            let stage = recovery.open_durable_cas_upload_admitted(&guard, id, &ctx.fingerprint)?;
            stage.ensure_publication_contract(
                &publication_key,
                req.expected_catalog_head.0.as_deref(),
            )?;
            stage
        }
        None => recovery.begin_durable_cas_upload_admitted(
            &guard,
            &ctx.fingerprint,
            "bundle-catalog-upload",
            &publication_key,
            req.expected_catalog_head.0.as_deref(),
        )?,
    };
    if stage.admitted_target_hash().is_some() {
        bail!("catalog upload session is already admitted");
    }
    let mut object_hashes = Vec::with_capacity(req.objects.len());
    for value in &req.objects {
        object_hashes.push(stage.store_object(&guard, &cas, value)?);
    }
    let mut blob_hashes = Vec::with_capacity(req.blobs_base64.len());
    for encoded in &req.blobs_base64 {
        if encoded.len() > MAX_INLINE_BLOB_BYTES.saturating_add(2) / 3 * 4 {
            bail!("inline catalog blob exceeds the bounded upload size");
        }
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .context("catalog upload contains invalid base64")?;
        if bytes.len() > MAX_INLINE_BLOB_BYTES {
            bail!("inline catalog blob exceeds the bounded upload size");
        }
        blob_hashes.push(stage.store_blob(&guard, &cas, &bytes)?);
    }
    for chunk in &req.blob_chunks {
        if chunk.total_size > MAX_BLOB_BYTES {
            bail!("catalog blob exceeds the bounded transport size");
        }
        if chunk.data_base64.len() > MAX_BLOB_CHUNK_BYTES.saturating_mul(2) {
            bail!("encoded catalog blob chunk exceeds the route-safe bound");
        }
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&chunk.data_base64)
            .context("catalog upload contains invalid chunk base64")?;
        if bytes.len() > MAX_BLOB_CHUNK_BYTES {
            bail!("catalog blob chunk exceeds the bounded upload size");
        }
        if stage.store_blob_chunk(
            &guard,
            &cas,
            &chunk.hash,
            chunk.total_size,
            chunk.offset,
            &bytes,
        )? {
            blob_hashes.push(chunk.hash.clone());
        }
    }
    Ok(serde_json::json!({
        "upload_session_id": stage.staging_id(),
        "expected_catalog_head": stage.expected_previous_hash(),
        "object_hashes": object_hashes,
        "blob_hashes": blob_hashes,
    }))
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StageLocalRequest {
    pub catalog_namespace: String,
    pub candidate_publication_attestation_hash: String,
    pub expected_catalog_head: ExpectedCatalogHead,
}

pub async fn stage_local(
    req: StageLocalRequest,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value> {
    ctx.require_verified()
        .map_err(|error| anyhow::anyhow!(error))?;
    let expected_catalog_head = req.expected_catalog_head.0;
    let upload_session_id = stage_local_catalog_closure(
        &state,
        LocalCatalogClosureStageRequest {
            authenticated_principal: ctx.fingerprint,
            catalog_namespace: req.catalog_namespace.clone(),
            candidate_attestation_hash: req.candidate_publication_attestation_hash.clone(),
            expected_current: expected_catalog_head.clone(),
        },
    )?;
    Ok(serde_json::json!({
        "catalog_namespace": req.catalog_namespace,
        "candidate_publication_attestation_hash": req.candidate_publication_attestation_hash,
        "expected_catalog_head": expected_catalog_head,
        "upload_session_id": upload_session_id,
    }))
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InspectRequest {
    pub catalog_namespace: String,
    #[serde(default)]
    pub catalog_publication_attestation_hash: Option<String>,
}

pub async fn inspect(req: InspectRequest, state: Arc<AppState>) -> Result<Value> {
    Ok(serde_json::to_value(inspect_catalog(
        &state,
        &req.catalog_namespace,
        req.catalog_publication_attestation_hash.as_deref(),
    )?)?)
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolveRequest {
    pub catalog_namespace: String,
    pub set_name: String,
    pub channel: String,
}

pub async fn resolve(req: ResolveRequest, state: Arc<AppState>) -> Result<Value> {
    let inspected = inspect_catalog(&state, &req.catalog_namespace, None)?;
    let coordinate = inspected
        .snapshot
        .set_channels
        .iter()
        .find(|entry| entry.set_name == req.set_name && entry.channel == req.channel)
        .context("requested curated-set channel is absent from the current catalog")?;
    Ok(serde_json::json!({
        "catalog_namespace": req.catalog_namespace,
        "catalog_publication_attestation_hash": inspected.catalog_publication_attestation_hash,
        "catalog_snapshot_hash": inspected.publication.snapshot_hash,
        "set_name": req.set_name,
        "channel": req.channel,
        "set_attestation_hash": coordinate.set_attestation_hash,
    }))
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExportRecoveryRequest {
    pub catalog_namespace: String,
}

pub async fn export_recovery(req: ExportRecoveryRequest, state: Arc<AppState>) -> Result<Value> {
    let inspected = inspect_catalog(&state, &req.catalog_namespace, None)?;
    let authority = state.state_store.pinned_state_authority()?;
    let _guard = authority.acquire_shared_guard()?;
    let cas = authority.cas_store()?;
    let limits = state
        .node_policy
        .require::<NodeObjectClosurePolicy>()?
        .closure_limits()?;
    let closure = ryeos_state::object_closure::collect_object_closure_with_cas_and_limits(
        &cas,
        [inspected.catalog_publication_attestation_hash.clone()],
        limits,
    )?;
    if !closure.is_complete() {
        bail!("cannot export an incomplete catalog recovery closure");
    }
    let policy = state.node_policy.require::<BundlePublicationPolicy>()?;
    Ok(serde_json::json!({
        "schema": "ryeos.bundle-catalog-recovery.v1",
        "kind": "bundle_catalog_recovery",
        "catalog_namespace": req.catalog_namespace,
        "catalog_publication_attestation_hash": inspected.catalog_publication_attestation_hash,
        "bundle_publication_policy_section_digest": policy.section_digest()?,
        "node_policy_generation_digest": state.node_policy.generation_digest(),
        "object_hashes": closure.object_hashes,
        "blob_hashes": closure.blob_hashes,
        "large_object_hashes": closure.large_object_hashes,
    }))
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RestoreGenesisRequest {
    pub catalog_namespace: String,
    pub catalog_publication_attestation_hash: String,
    pub upload_session_id: String,
    pub bundle_publication_policy_section_digest: String,
    pub node_policy_generation_digest: String,
}

pub async fn restore_genesis(
    req: RestoreGenesisRequest,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value> {
    ctx.require_verified()
        .map_err(|error| anyhow::anyhow!(error))?;
    let policy = state.node_policy.require::<BundlePublicationPolicy>()?;
    if req.bundle_publication_policy_section_digest != policy.section_digest()?
        || req.node_policy_generation_digest != state.node_policy.generation_digest()
    {
        bail!("recovery manifest is bound to another node-policy generation");
    }
    let current = state.state_store.with_state_db(|db| {
        db.read_generic_head_ref(BUNDLE_CATALOG_HEAD_NAMESPACE, &req.catalog_namespace)
    })?;
    if current.is_some() {
        bail!("catalog genesis restore is forbidden while a head exists");
    }
    let outcome = publish_catalog(
        &state,
        CatalogPublicationRequest {
            authenticated_principal: ctx.fingerprint,
            catalog_namespace: req.catalog_namespace,
            candidate_attestation_hash: req.catalog_publication_attestation_hash,
            expected_current: None,
            upload_session_id: req.upload_session_id,
        },
    )?;
    Ok(serde_json::to_value(outcome)?)
}

macro_rules! descriptor {
    ($name:ident, $service:literal, $endpoint:literal, $cap:literal, $request:ty, $handler:ident $(, $context:ident)?) => {
        pub const $name: ServiceDescriptor = ServiceDescriptor {
            service_ref: $service,
            endpoint: $endpoint,
            availability: ServiceAvailability::DaemonOnly,
            required_caps: &[$cap],
            handler: |params, ctx, state| Box::pin(async move {
                let request: $request = crate::handler_error::parse_request(params)?;
                descriptor!(@call $handler, request, ctx, state $(, $context)?)
            }),
        };
    };
    (@call $handler:ident, $request:ident, $ctx:ident, $state:ident, context) => { $handler($request, $ctx, $state).await };
    (@call $handler:ident, $request:ident, $ctx:ident, $state:ident) => {{ let _ = $ctx; $handler($request, $state).await }};
}

descriptor!(
    UPLOAD,
    "service:bundle-catalog/upload",
    "bundle_catalog.upload",
    "ryeos.execute.service.bundle-catalog/upload",
    UploadRequest,
    upload,
    context
);
descriptor!(
    STAGE_LOCAL,
    "service:bundle-catalog/stage-local",
    "bundle_catalog.stage_local",
    "ryeos.execute.service.bundle-catalog/stage-local",
    StageLocalRequest,
    stage_local,
    context
);
descriptor!(
    INSPECT,
    "service:bundle-catalog/inspect",
    "bundle_catalog.inspect",
    "ryeos.execute.service.bundle-catalog/inspect",
    InspectRequest,
    inspect
);
descriptor!(
    RESOLVE,
    "service:bundle-catalog/resolve",
    "bundle_catalog.resolve",
    "ryeos.execute.service.bundle-catalog/resolve",
    ResolveRequest,
    resolve
);
descriptor!(
    EXPORT_RECOVERY,
    "service:bundle-catalog/export-recovery",
    "bundle_catalog.export_recovery",
    "ryeos.execute.service.bundle-catalog/export-recovery",
    ExportRecoveryRequest,
    export_recovery
);
descriptor!(
    RESTORE_GENESIS,
    "service:bundle-catalog/restore-genesis",
    "bundle_catalog.restore_genesis",
    "ryeos.execute.service.bundle-catalog/restore-genesis",
    RestoreGenesisRequest,
    restore_genesis,
    context
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptors_expand_with_and_without_handler_context() {
        for (descriptor, name) in [
            (UPLOAD, "upload"),
            (STAGE_LOCAL, "stage-local"),
            (INSPECT, "inspect"),
            (RESOLVE, "resolve"),
            (EXPORT_RECOVERY, "export-recovery"),
            (RESTORE_GENESIS, "restore-genesis"),
        ] {
            assert_eq!(
                descriptor.service_ref,
                format!("service:bundle-catalog/{name}")
            );
            assert_eq!(descriptor.required_caps.len(), 1);
            assert_eq!(
                descriptor.required_caps[0],
                format!("ryeos.execute.service.bundle-catalog/{name}")
            );
        }
    }
}
