//! `objects/stage-chunk` — append one bounded chunk to a durable staging
//! capability owned by the authenticated operator.
//!
//! Two namespaces share the capability. A `blob` chunk lands in the ordinary
//! CAS through the durable blob-part path. A `large_object` chunk lands in the
//! large-object store and is checked against the commitments (size, sidecar
//! chunk size, per-chunk hashes) of an already-staged large-content manifest;
//! the declared hash is never trusted on its own.
//!
//! Request: `{ staging_id, namespace: "blob"|"large_object", hash, total_size,
//! offset, data (base64), manifest_hash? }`.
//! Response: `{ staging_id, namespace, hash, staged_bytes, complete }`.

use std::sync::Arc;

use anyhow::{Context as _, Result, bail};
use base64::Engine as _;
use ryeos_app::handler_context::HandlerContext;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;
use ryeos_node_runtime::node_policy::sections::external_content::ExternalContentImportPolicyRecord;
use ryeos_state::large_object_store::LargeObjectExpectation;
use serde::Deserialize;
use serde_json::Value;

use crate::registry::ServiceDescriptor;

/// Decoded bytes accepted per call, matching the read side.
pub const MAX_STAGE_CHUNK_BYTES: usize = super::objects_get::MAX_BLOB_CHUNK_BYTES;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Namespace {
    Blob,
    LargeObject,
}

impl Namespace {
    fn as_str(self) -> &'static str {
        match self {
            Self::Blob => "blob",
            Self::LargeObject => "large_object",
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub staging_id: String,
    pub namespace: Namespace,
    pub hash: String,
    pub total_size: u64,
    pub offset: u64,
    /// Base64 of one bounded sequential chunk.
    pub data: String,
    /// Staged large-content manifest that declares this large object.
    /// Required for `large_object`, refused for `blob`.
    #[serde(default)]
    pub manifest_hash: Option<String>,
}

impl Request {
    /// Shape checks that need no store. Returns the decoded chunk.
    fn validate(&self) -> Result<Vec<u8>> {
        if self.staging_id.is_empty() || self.staging_id.len() > 256 {
            bail!("objects/stage-chunk requires a staging_id issued by the server");
        }
        if !is_canonical_hash(&self.hash) {
            bail!("objects/stage-chunk hash is not canonical SHA-256");
        }
        if self.total_size == 0 {
            bail!("objects/stage-chunk total_size must be positive");
        }
        match (self.namespace, self.manifest_hash.as_deref()) {
            (Namespace::Blob, Some(_)) => {
                bail!(
                    "objects/stage-chunk manifest_hash is valid only for the large_object namespace"
                )
            }
            (Namespace::LargeObject, None) => {
                bail!("objects/stage-chunk large_object chunks require a manifest_hash")
            }
            (Namespace::LargeObject, Some(manifest)) if !is_canonical_hash(manifest) => {
                bail!("objects/stage-chunk manifest_hash is not canonical SHA-256")
            }
            _ => {}
        }
        if self.namespace == Namespace::Blob && self.total_size > super::objects_put::MAX_BLOB_BYTES
        {
            bail!(
                "blob exceeds the {} byte per-blob limit",
                super::objects_put::MAX_BLOB_BYTES
            );
        }
        if self.data.len() > MAX_STAGE_CHUNK_BYTES.saturating_mul(2) {
            bail!("encoded stage chunk exceeds the route-safe bound");
        }
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&self.data)
            .map_err(|error| anyhow::anyhow!("invalid base64 in stage chunk: {error}"))?;
        if bytes.is_empty() {
            bail!("objects/stage-chunk carries an empty chunk");
        }
        if bytes.len() > MAX_STAGE_CHUNK_BYTES {
            bail!("stage chunk exceeds {MAX_STAGE_CHUNK_BYTES} bytes");
        }
        Ok(bytes)
    }
}

fn is_canonical_hash(hash: &str) -> bool {
    lillux::valid_hash(hash) && !hash.bytes().any(|byte| byte.is_ascii_uppercase())
}

pub async fn handle(req: Request, ctx: HandlerContext, state: Arc<AppState>) -> Result<Value> {
    ryeos_app::operator_authority::require_admitted_operator(&state, &ctx)?;
    if ctx.fingerprint.is_empty() {
        bail!("objects/stage-chunk requires an authenticated owner");
    }
    let bytes = req.validate()?;
    let owner = ctx.fingerprint.clone();
    tokio::task::spawn_blocking(move || stage_chunk_blocking(&state, &owner, &req, &bytes))
        .await
        .context("stage chunk task stopped")?
}

fn stage_chunk_blocking(
    state: &AppState,
    owner: &str,
    req: &Request,
    bytes: &[u8],
) -> Result<Value> {
    let authority = state.state_store.pinned_state_authority()?;
    let guard = authority.acquire_shared_guard()?;
    let cas = authority.cas_store()?;
    let _permit = state
        .write_barrier
        .acquire_with_timeout(ryeos_app::write_barrier::ONLINE_WRITE_PERMIT_TIMEOUT)
        .map_err(|error| anyhow::anyhow!("cannot acquire stage chunk write permit: {error}"))?;
    let recovery = authority.require_recovery()?;
    // Ownership is the authenticated operator: a stage minted for another
    // principal is refused by the reopen itself.
    let mut stage = recovery.open_durable_cas_upload_admitted(&guard, &req.staging_id, owner)?;
    if let Some(target) = stage.admitted_target_hash() {
        bail!(
            "durable upload stage is already admitted for publication target {target}; it accepts no further chunks"
        );
    }
    let length = u64::try_from(bytes.len())?;
    let (staged_bytes, complete) = match req.namespace {
        Namespace::Blob => {
            let complete = stage.store_blob_chunk(
                &guard,
                &cas,
                &req.hash,
                req.total_size,
                req.offset,
                bytes,
            )?;
            // The blob path reports completion only; before completion the
            // accepted prefix is exactly the end of this chunk.
            let staged = if complete {
                req.total_size
            } else {
                req.offset
                    .checked_add(length)
                    .context("stage chunk range overflow")?
            };
            (staged, complete)
        }
        Namespace::LargeObject => {
            let manifest_hash = req
                .manifest_hash
                .as_deref()
                .context("large_object chunks require a manifest_hash")?;
            if !matches!(
                stage.publication_key(),
                ryeos_state::DurableCasPublicationKey::ExternalContentImport { .. }
            ) {
                anyhow::bail!("large-object chunks are staged only into a product-offer stage");
            }
            stage.ensure_protects_object(manifest_hash)?;
            let manifest_value = cas
                .get_object(manifest_hash)?
                .with_context(|| format!("staged manifest {manifest_hash} is absent"))?;
            let manifest = ryeos_state::objects::ExternalLargeContentManifestObject::from_value(
                &manifest_value,
            )
            .with_context(|| {
                format!("staged object {manifest_hash} is not a large-content manifest")
            })?;
            let expectation = expectation_for(&manifest, &req.hash, req.total_size)?;
            let store = authority.large_object_store()?;
            // Every chunk that opens a new sidecar chunk is admitted against
            // the budget and free space for the bytes still to come; the
            // in-flight staging already on disk is counted as stored.
            let already_held = store.staged_bytes(&req.hash)?;
            let resident = already_held == req.total_size;
            if !resident && req.offset % expectation.chunk_size == 0 {
                let policy = state
                    .node_policy
                    .require::<ExternalContentImportPolicyRecord>()?;
                require_store_room(
                    &store,
                    req.total_size.saturating_sub(already_held),
                    policy.limits.store_budget_bytes,
                    policy.limits.minimum_free_bytes,
                )?;
            }
            let progress = stage.store_large_object_chunk(
                &guard,
                &store,
                &req.hash,
                &expectation,
                req.offset,
                bytes,
            )?;
            (progress.staged_bytes, progress.complete)
        }
    };
    Ok(serde_json::json!({
        "staging_id": stage.staging_id(),
        "namespace": req.namespace.as_str(),
        "hash": req.hash,
        "staged_bytes": staged_bytes,
        "complete": complete,
    }))
}

/// Build the sealed expectation for `file_sha256` from the manifest entry that
/// names it, refusing a request whose declared size contradicts the manifest.
fn expectation_for(
    manifest: &ryeos_state::objects::ExternalLargeContentManifestObject,
    file_sha256: &str,
    total_size: u64,
) -> Result<LargeObjectExpectation> {
    let entry = manifest
        .entries
        .iter()
        .find(|entry| entry.file_sha256.as_deref() == Some(file_sha256))
        .with_context(|| {
            format!("the staged manifest has no large-object entry with file_sha256 {file_sha256}")
        })?;
    let (Some(size), Some(chunk_size)) = (entry.size, entry.chunk_size) else {
        bail!("the manifest entry for {file_sha256} carries no size or sidecar chunk size");
    };
    if size != total_size {
        bail!(
            "stage chunk total_size {total_size} contradicts the manifest size {size} for {file_sha256}"
        );
    }
    Ok(LargeObjectExpectation {
        size,
        chunk_size,
        chunk_hashes: entry.chunk_hashes.clone(),
    })
}

/// The same budget and free-space admission the maintenance sweep enforces,
/// applied before the first byte of a new large object is accepted.
fn require_store_room(
    store: &ryeos_state::LargeObjectStore,
    incoming_bytes: u64,
    store_budget_bytes: u64,
    minimum_free_bytes: u64,
) -> Result<()> {
    let stored = store
        .total_stored_bytes()?
        .checked_add(store.staging_bytes()?)
        .context("large-store byte count overflow")?;
    let after = stored
        .checked_add(incoming_bytes)
        .context("large-store budget overflow")?;
    if after > store_budget_bytes {
        bail!(
            "large object of {incoming_bytes} bytes would take the store to {after} bytes, over the {store_budget_bytes} byte budget"
        );
    }
    let capacity = store.filesystem_capacity()?;
    let required = minimum_free_bytes
        .checked_add(incoming_bytes)
        .context("large-store free-space requirement overflow")?;
    if capacity.available_bytes < required {
        bail!(
            "large object of {incoming_bytes} bytes needs {required} available bytes including the reserve, observed {}",
            capacity.available_bytes
        );
    }
    Ok(())
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:objects/stage-chunk",
    endpoint: "objects.stage-chunk",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.objects/stage-chunk"],
    handler: |params, ctx, state| {
        Box::pin(
            async move { handle(crate::handler_error::parse_request(params)?, ctx, state).await },
        )
    },
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authored_service_schema_matches_the_typed_request() {
        let descriptor: serde_json::Value = serde_yaml::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../bundles/core/.ai/services/objects/stage-chunk.yaml"
        )))
        .unwrap();
        assert_eq!(descriptor["endpoint"], DESCRIPTOR.endpoint);
        assert_eq!(
            descriptor["required_caps"],
            serde_json::json!(DESCRIPTOR.required_caps)
        );
        assert_eq!(
            descriptor["schema"],
            serde_json::json!({
                "staging_id": "string",
                "namespace": "string",
                "hash": "string",
                "total_size": "integer",
                "offset": "integer",
                "data": "string",
                "manifest_hash": "string?",
            })
        );
        // Every authored field is a field of the typed request: the request
        // refuses unknown fields, so a field missing from it would fail here.
        let mut coordinates = serde_json::Map::new();
        for (field, kind) in descriptor["schema"].as_object().unwrap() {
            let kind = kind.as_str().unwrap();
            if kind.ends_with('?') {
                continue;
            }
            let value = match kind.trim_end_matches('?') {
                "integer" => serde_json::json!(1),
                "object" => serde_json::json!({}),
                "object[]" => serde_json::json!([]),
                _ if field == "namespace" => serde_json::json!("blob"),
                _ => serde_json::json!("x"),
            };
            coordinates.insert(field.clone(), value);
        }
        let mut with_unknown = coordinates.clone();
        with_unknown.insert("not_a_field".to_owned(), serde_json::json!(1));
        assert!(serde_json::from_value::<Request>(serde_json::Value::Object(coordinates)).is_ok());
        assert!(
            serde_json::from_value::<Request>(serde_json::Value::Object(with_unknown)).is_err()
        );
    }
    use ryeos_state::objects::{
        EXTERNAL_LARGE_CONTENT_MANIFEST_KIND, EXTERNAL_LARGE_CONTENT_SCHEMA,
        ExternalContentManifestEntryKind, ExternalLargeContentManifestEntry,
        ExternalLargeContentManifestObject,
    };

    fn request(namespace: &str, manifest: Option<&str>, data: &[u8]) -> Value {
        let mut value = serde_json::json!({
            "staging_id": "upload-abc",
            "namespace": namespace,
            "hash": "ab".repeat(32),
            "total_size": 1024,
            "offset": 0,
            "data": base64::engine::general_purpose::STANDARD.encode(data),
        });
        if let Some(manifest) = manifest {
            value["manifest_hash"] = Value::String(manifest.to_owned());
        }
        value
    }

    fn parse(value: Value) -> Request {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn descriptor_names_the_stage_chunk_service() {
        assert_eq!(DESCRIPTOR.service_ref, "service:objects/stage-chunk");
        assert_eq!(DESCRIPTOR.endpoint, "objects.stage-chunk");
        assert_eq!(
            DESCRIPTOR.required_caps,
            ["ryeos.execute.service.objects/stage-chunk"]
        );
        assert!(matches!(
            DESCRIPTOR.availability,
            ServiceAvailability::DaemonOnly
        ));
    }

    #[test]
    fn request_shape_refuses_unknown_fields_namespaces_and_missing_coordinates() {
        let valid = request("blob", None, b"abc");
        assert_eq!(parse(valid.clone()).validate().unwrap(), b"abc");
        let mut unknown = valid.clone();
        unknown["extra"] = Value::Bool(true);
        assert!(serde_json::from_value::<Request>(unknown).is_err());
        let mut bad_namespace = valid.clone();
        bad_namespace["namespace"] = Value::String("object".to_owned());
        assert!(serde_json::from_value::<Request>(bad_namespace).is_err());
        for field in [
            "staging_id",
            "namespace",
            "hash",
            "total_size",
            "offset",
            "data",
        ] {
            let mut missing = valid.clone();
            missing.as_object_mut().unwrap().remove(field);
            assert!(
                serde_json::from_value::<Request>(missing).is_err(),
                "{field}"
            );
        }
    }

    #[test]
    fn request_validation_refuses_bad_hashes_sizes_and_chunks() {
        let manifest = "cd".repeat(32);
        parse(request("large_object", Some(&manifest), b"abc"))
            .validate()
            .unwrap();
        let refused = |value: Value, needle: &str| {
            let error = parse(value).validate().unwrap_err().to_string();
            assert!(error.contains(needle), "{error}");
        };
        let mut upper = request("blob", None, b"abc");
        upper["hash"] = Value::String("AB".repeat(32));
        refused(upper, "hash is not canonical");
        let mut zero = request("blob", None, b"abc");
        zero["total_size"] = Value::from(0);
        refused(zero, "total_size must be positive");
        refused(request("blob", None, b""), "empty chunk");
        refused(
            request("blob", None, &vec![0_u8; MAX_STAGE_CHUNK_BYTES + 1]),
            "exceeds",
        );
        let mut garbage = request("blob", None, b"abc");
        garbage["data"] = Value::String("***".to_owned());
        refused(garbage, "invalid base64");
    }

    #[test]
    fn request_validation_enforces_the_manifest_hash_rule_per_namespace() {
        let manifest = "cd".repeat(32);
        let error = parse(request("blob", Some(&manifest), b"abc"))
            .validate()
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("valid only for the large_object namespace"),
            "{error}"
        );
        let error = parse(request("large_object", None, b"abc"))
            .validate()
            .unwrap_err()
            .to_string();
        assert!(error.contains("require a manifest_hash"), "{error}");
        let error = parse(request("large_object", Some("short"), b"abc"))
            .validate()
            .unwrap_err()
            .to_string();
        assert!(error.contains("manifest_hash is not canonical"), "{error}");
    }

    fn manifest_with(file_sha256: &str, size: u64) -> ExternalLargeContentManifestObject {
        ExternalLargeContentManifestObject {
            schema: EXTERNAL_LARGE_CONTENT_SCHEMA.to_owned(),
            kind: EXTERNAL_LARGE_CONTENT_MANIFEST_KIND.to_owned(),
            entries: vec![ExternalLargeContentManifestEntry {
                path: "content".to_owned(),
                kind: ExternalContentManifestEntryKind::File,
                mode: None,
                blob_hash: None,
                file_sha256: Some(file_sha256.to_owned()),
                size: Some(size),
                chunk_size: Some(1024),
                chunk_hashes: vec!["ef".repeat(32)],
                target: None,
            }],
            entry_count: 1,
            total_bytes: size,
        }
    }

    #[test]
    fn expectation_comes_from_the_manifest_entry_and_refuses_contradictions() {
        let hash = "ab".repeat(32);
        let manifest = manifest_with(&hash, 1000);
        let expectation = expectation_for(&manifest, &hash, 1000).unwrap();
        assert_eq!(expectation.size, 1000);
        assert_eq!(expectation.chunk_size, 1024);
        assert_eq!(expectation.chunk_hashes, vec!["ef".repeat(32)]);
        let error = expectation_for(&manifest, &hash, 999)
            .unwrap_err()
            .to_string();
        assert!(error.contains("contradicts the manifest size"), "{error}");
        let other = "12".repeat(32);
        let error = expectation_for(&manifest, &other, 1000)
            .unwrap_err()
            .to_string();
        assert!(error.contains("no large-object entry"), "{error}");
    }
}
