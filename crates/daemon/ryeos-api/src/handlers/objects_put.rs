//! `objects/put` — store blobs and objects in the CAS.
//!
//! Batch store: accepts `{ "blobs": [{"data": "<base64>"}], "objects": [{"value": {...}}] }`.
//! Returns `{ "blob_hashes": ["abc"], "object_hashes": ["def"] }`.
//!
//! For blobs, the client base64-encodes the raw bytes. The server
//! stores via `CasStore::store_blob` which hashes and dedups.
//! For objects, the client sends the full JSON value.

use std::sync::Arc;

use anyhow::Result;
use base64::Engine as _;
use serde_json::Value;

use crate::handler_context::HandlerContext;
use crate::registry::ServiceDescriptor;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BlobEntry {
    /// Base64-encoded raw bytes.
    pub data: String,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BlobChunkEntry {
    pub hash: String,
    pub total_size: u64,
    pub offset: u64,
    /// Base64-encoded bytes for one bounded sequential chunk.
    pub data: String,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObjectEntry {
    /// JSON value to store as a CAS object.
    pub value: Value,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    /// Server-issued durable publication capability. Omit only on the first
    /// batch; the server mints and returns one before storing any CAS bytes.
    #[serde(default)]
    pub staging_id: Option<String>,
    /// Canonical publication target for this upload session.
    pub project_path: String,
    #[serde(default)]
    pub blobs: Vec<BlobEntry>,
    #[serde(default)]
    pub blob_chunks: Vec<BlobChunkEntry>,
    #[serde(default)]
    pub objects: Vec<ObjectEntry>,
}

/// Per-blob decoded-size ceiling. Route body limits bound each request
/// as a whole; this additionally keeps an `objects/put` grant from
/// pushing a single oversized blob through any route configured with a
/// generous body cap.
pub const MAX_BLOB_BYTES: u64 = 16 * 1024 * 1024 * 1024;
pub const MAX_BLOB_CHUNK_BYTES: usize = 512 * 1024;
pub const MAX_INLINE_BLOB_BYTES: usize = 16 * 1024 * 1024;

pub async fn handle(req: Request, ctx: HandlerContext, state: Arc<AppState>) -> Result<Value> {
    ctx.require_verified()
        .map_err(|error| anyhow::anyhow!(error))?;
    let authority = state
        .state_store
        .with_state_db(|db| db.pinned_authority())?;
    let cas_guard = authority.acquire_shared_guard()?;
    let cas = authority.cas_store()?;
    let _permit = state
        .write_barrier
        .try_acquire()
        .map_err(|e| anyhow::anyhow!("cannot acquire CAS write permit: {e}"))?;
    let recovery = authority.require_recovery()?;
    let canonical_project_path =
        ryeos_executor::execution::project_source::canonical_project_ref(&req.project_path)
            .map_err(|error| anyhow::anyhow!("objects.put: {error}"))?;
    let principal_key = ryeos_state::refs::principal_storage_key(&ctx.fingerprint)?;
    let project_hash = lillux::sha256_hex(canonical_project_path.as_bytes());
    let publication_key =
        ryeos_state::DurableCasPublicationKey::project_head(principal_key, &project_hash)?;
    let mut stage = match req.staging_id.as_deref() {
        Some(staging_id) => {
            let stage = recovery.open_durable_cas_upload_admitted(
                &cas_guard,
                staging_id,
                &ctx.fingerprint,
            )?;
            let expected = stage.expected_previous_hash().map(str::to_string);
            stage.ensure_publication_contract(&publication_key, expected.as_deref())?;
            stage
        }
        None => {
            let expected_previous_hash = state
                .state_store
                .with_state_db(|db| db.read_project_head(principal_key, &project_hash))?;
            recovery.begin_durable_cas_upload_admitted(
                &cas_guard,
                &ctx.fingerprint,
                "project-push",
                &publication_key,
                expected_previous_hash.as_deref(),
            )?
        }
    };
    if let Some(target) = stage.admitted_target_hash() {
        anyhow::bail!("durable upload session is already admitted for publication target {target}");
    }

    let mut blob_hashes = Vec::with_capacity(req.blobs.len());
    for blob in &req.blobs {
        // Cheap base64-length bound before decoding allocates anything.
        let max_encoded = MAX_INLINE_BLOB_BYTES.saturating_add(2) / 3 * 4;
        if blob.data.len() > max_encoded {
            anyhow::bail!(
                "inline blob exceeds {MAX_INLINE_BLOB_BYTES} bytes; use bounded blob_chunks"
            );
        }
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&blob.data)
            .map_err(|e| anyhow::anyhow!("invalid base64 in blob data: {e}"))?;
        let hash = stage.store_blob(&cas_guard, &cas, &bytes)?;
        blob_hashes.push(hash);
    }
    for chunk in &req.blob_chunks {
        if chunk.total_size > MAX_BLOB_BYTES {
            anyhow::bail!("blob exceeds the {} byte per-blob limit", MAX_BLOB_BYTES);
        }
        if chunk.data.len() > MAX_BLOB_CHUNK_BYTES.saturating_mul(2) {
            anyhow::bail!("encoded blob chunk exceeds the route-safe bound");
        }
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&chunk.data)
            .map_err(|e| anyhow::anyhow!("invalid base64 in blob chunk: {e}"))?;
        if bytes.len() > MAX_BLOB_CHUNK_BYTES {
            anyhow::bail!("blob chunk exceeds {MAX_BLOB_CHUNK_BYTES} bytes");
        }
        if stage.store_blob_chunk(
            &cas_guard,
            &cas,
            &chunk.hash,
            chunk.total_size,
            chunk.offset,
            &bytes,
        )? {
            blob_hashes.push(chunk.hash.clone());
        }
    }

    let mut object_hashes = Vec::with_capacity(req.objects.len());
    for obj in &req.objects {
        let hash = stage.store_object(&cas_guard, &cas, &obj.value)?;
        object_hashes.push(hash);
    }

    Ok(serde_json::json!({
        "staging_id": stage.staging_id(),
        "expected_previous_hash": stage.expected_previous_hash(),
        "blob_hashes": blob_hashes,
        "object_hashes": object_hashes,
    }))
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:objects/put",
    endpoint: "objects.put",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.objects/put"],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: Request = crate::handler_error::parse_request(params)?;
            handle(req, ctx, state).await
        })
    },
};
