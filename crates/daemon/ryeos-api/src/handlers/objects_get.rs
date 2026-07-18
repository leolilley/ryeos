//! `objects/get` — retrieve blobs and objects from the CAS.
//!
//! Batch get accepts typed object and blob hash sets. The namespace is part of
//! the request so an equal digest present in both namespaces is never resolved
//! by server-side precedence.
//! Returns `{ "entries": [{"hash": "abc", "kind": "blob", "data": "<base64>"}, ...] }`.
//!
//! Blobs return base64-encoded data. Objects return the JSON value.
//! Missing hashes are returned with `kind: "missing"`.

use std::io::{Read as _, Seek as _};
use std::sync::Arc;

use anyhow::Result;
use base64::Engine as _;
use serde_json::Value;

use crate::registry::ServiceDescriptor;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    #[serde(
        default,
        deserialize_with = "ryeos_runtime::scalar_or_vec::deserialize"
    )]
    pub object_hashes: Vec<String>,
    #[serde(
        default,
        deserialize_with = "ryeos_runtime::scalar_or_vec::deserialize"
    )]
    pub blob_hashes: Vec<String>,
    #[serde(default)]
    pub blob_chunk: Option<BlobChunkRequest>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BlobChunkRequest {
    pub hash: String,
    pub offset: u64,
    pub length: usize,
}

pub const MAX_BLOB_CHUNK_BYTES: usize = 512 * 1024;
pub const MAX_INLINE_BLOB_BYTES: u64 = 16 * 1024 * 1024;
pub const MAX_INLINE_OBJECT_BYTES: u64 = 32 * 1024 * 1024;

pub async fn handle(req: Request, state: Arc<AppState>) -> Result<Value> {
    validate_typed_hash_request(&req.object_hashes, &req.blob_hashes)?;
    let cas_read = state.acquire_cas_read()?;
    let cas = cas_read.cas();

    if let Some(chunk) = req.blob_chunk {
        if !req.object_hashes.is_empty() || !req.blob_hashes.is_empty() {
            anyhow::bail!("objects/get cannot mix hash batches with a blob chunk request");
        }
        if chunk.length == 0 || chunk.length > MAX_BLOB_CHUNK_BYTES {
            anyhow::bail!("objects/get blob chunk length is outside the bounded range");
        }
        if !is_canonical_sha256(&chunk.hash) {
            anyhow::bail!("objects/get blob chunk hash is not canonical SHA-256");
        }
        let Some((mut source, total_size)) = cas.open_blob(&chunk.hash)? else {
            return Ok(serde_json::json!({
                "blob_chunk": {
                    "hash": chunk.hash,
                    "kind": "missing"
                }
            }));
        };
        if chunk.offset > total_size {
            anyhow::bail!("objects/get blob chunk offset exceeds blob size");
        }
        source.seek(std::io::SeekFrom::Start(chunk.offset))?;
        let remaining = total_size - chunk.offset;
        let requested = u64::try_from(chunk.length)?.min(remaining);
        let mut bytes = vec![0_u8; usize::try_from(requested)?];
        source.read_exact(&mut bytes)?;
        return Ok(serde_json::json!({
            "blob_chunk": {
                "hash": chunk.hash,
                "kind": "blob_chunk",
                "offset": chunk.offset,
                "total_size": total_size,
                "eof": chunk.offset + requested == total_size,
                "data": base64::engine::general_purpose::STANDARD.encode(bytes),
            }
        }));
    }
    if req.object_hashes.is_empty() && req.blob_hashes.is_empty() {
        anyhow::bail!(
            "objects/get requires typed object_hashes/blob_hashes or one blob chunk request"
        );
    }

    let mut entries = Vec::with_capacity(req.object_hashes.len() + req.blob_hashes.len());

    for hash in &req.object_hashes {
        if let Some((mut source, size)) = cas.open_object(hash)? {
            if size > MAX_INLINE_OBJECT_BYTES {
                anyhow::bail!("object {hash} exceeds the inline response limit");
            }
            let mut bytes = vec![0_u8; usize::try_from(size)?];
            source.read_exact(&mut bytes)?;
            if lillux::sha256_hex(&bytes) != *hash {
                anyhow::bail!("CAS object {hash} failed content-address verification");
            }
            let value: Value = serde_json::from_slice(&bytes)?;
            if lillux::canonical_json(&value)?.as_bytes() != bytes {
                anyhow::bail!("CAS object {hash} is not canonical JSON");
            }
            entries.push(serde_json::json!({
                "hash": hash,
                "kind": "object",
                "value": value,
            }));
        } else {
            entries.push(serde_json::json!({
                "hash": hash,
                "kind": "missing_object",
            }));
        }
    }
    for hash in &req.blob_hashes {
        if let Some((mut source, size)) = cas.open_blob(hash)? {
            if size > MAX_INLINE_BLOB_BYTES {
                anyhow::bail!(
                    "blob {hash} exceeds the inline response limit; use a bounded blob_chunk request"
                );
            }
            let mut data = vec![0_u8; usize::try_from(size)?];
            source.read_exact(&mut data)?;
            let encoded = base64::engine::general_purpose::STANDARD.encode(&data);
            entries.push(serde_json::json!({
                "hash": hash,
                "kind": "blob",
                "data": encoded,
            }));
        } else {
            entries.push(serde_json::json!({
                "hash": hash,
                "kind": "missing_blob",
            }));
        }
    }

    Ok(serde_json::json!({
        "entries": entries,
    }))
}

fn validate_typed_hash_request(object_hashes: &[String], blob_hashes: &[String]) -> Result<()> {
    for (namespace, hashes) in [("object", object_hashes), ("blob", blob_hashes)] {
        let mut seen = std::collections::BTreeSet::new();
        for hash in hashes {
            if !is_canonical_sha256(hash) {
                anyhow::bail!("objects/get contains an invalid {namespace} hash");
            }
            if !seen.insert(hash) {
                anyhow::bail!("objects/get contains a duplicate {namespace} hash");
            }
        }
    }
    Ok(())
}

fn is_canonical_sha256(hash: &str) -> bool {
    lillux::cas::valid_hash(hash)
        && hash
            .as_bytes()
            .iter()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte))
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:objects/get",
    endpoint: "objects.get",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.objects/get"],
    handler: |params, _ctx, state| {
        Box::pin(async move {
            let req: Request = crate::handler_error::parse_request(params)?;
            handle(req, state).await
        })
    },
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_hash_requests_require_canonical_lowercase_sha256() {
        let lower = "ab".repeat(32);
        let upper = lower.to_ascii_uppercase();
        assert!(validate_typed_hash_request(std::slice::from_ref(&lower), &[]).is_ok());
        assert!(validate_typed_hash_request(std::slice::from_ref(&upper), &[]).is_err());
        assert!(validate_typed_hash_request(&[], &["not-a-hash".to_owned()]).is_err());
    }
}
