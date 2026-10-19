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
pub const MAX_RAW_OBJECT_BYTES: usize = 32 * 1024 * 1024;
pub const MAX_CAPSULE_OBJECT_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_OBJECT_PUT_BODY_BYTES: usize = 100 * 1024 * 1024;
pub const MAX_OBJECT_PUT_ENTRIES: usize = 100_000;

fn add_canonical_len(total: usize, amount: usize, limit: usize) -> anyhow::Result<usize> {
    let next = total.saturating_add(amount);
    anyhow::ensure!(next <= limit, "canonical JSON exceeds {limit} bytes");
    Ok(next)
}

fn accumulate_request_payload(total: usize, amount: usize) -> anyhow::Result<usize> {
    let next = total.saturating_add(amount);
    anyhow::ensure!(
        next <= MAX_OBJECT_PUT_BODY_BYTES,
        "objects.put canonical payload exceeds 100 MiB"
    );
    Ok(next)
}

fn canonical_string_len(value: &str, limit: usize) -> anyhow::Result<usize> {
    let mut len = 2usize;
    for ch in value.chars() {
        let n = match ch {
            '"' | '\\' | '\x08' | '\x0C' | '\n' | '\r' | '\t' => 2,
            c if c < '\x20' => 6,
            c if c.is_ascii() => 1,
            c if (c as u32) <= 0xFFFF => 6,
            _ => 12,
        };
        len = add_canonical_len(len, n, limit)?;
    }
    anyhow::ensure!(len <= limit, "canonical JSON string exceeds {limit} bytes");
    Ok(len)
}

/// Compute the byte length of lillux's canonical encoding without allocating
/// that encoding. The length rules mirror `lillux::canonical_json`; the CAS
/// writer remains the sole producer of persisted/hash bytes.
fn bounded_canonical_json_len(value: &Value, limit: usize) -> anyhow::Result<usize> {
    fn visit(value: &Value, limit: usize) -> anyhow::Result<usize> {
        match value {
            Value::Null => Ok(4),
            Value::Bool(true) => Ok(4),
            Value::Bool(false) => Ok(5),
            Value::Number(number) => {
                let rendered = number.to_string();
                let normalized: serde_json::Number = serde_json::from_str(&rendered)?;
                let len = normalized.to_string().len();
                anyhow::ensure!(len <= limit, "canonical JSON number exceeds {limit} bytes");
                Ok(len)
            }
            Value::String(string) => canonical_string_len(string, limit),
            Value::Array(items) => {
                let mut len = 2usize;
                for (index, item) in items.iter().enumerate() {
                    if index != 0 {
                        len = add_canonical_len(len, 1, limit)?;
                    }
                    len = add_canonical_len(len, visit(item, limit)?, limit)?;
                }
                Ok(len)
            }
            Value::Object(map) => {
                let mut len = 2usize;
                for (index, (key, item)) in map.iter().enumerate() {
                    if index != 0 {
                        len = add_canonical_len(len, 1, limit)?;
                    }
                    len = add_canonical_len(len, canonical_string_len(key, limit)?, limit)?;
                    len = add_canonical_len(len, 1, limit)?;
                    len = add_canonical_len(len, visit(item, limit)?, limit)?;
                }
                Ok(len)
            }
        }
    }
    let len = visit(value, limit)?;
    anyhow::ensure!(len <= limit, "canonical JSON exceeds {limit} bytes");
    Ok(len)
}

fn decode_inline_blob_bounded(data: &str) -> anyhow::Result<usize> {
    let max_encoded = MAX_INLINE_BLOB_BYTES.saturating_add(2) / 3 * 4;
    anyhow::ensure!(
        data.len() <= max_encoded,
        "inline blob exceeds 16 MiB; use blob_chunks"
    );
    let decoded = base64::engine::general_purpose::STANDARD.decode(data)?;
    anyhow::ensure!(
        decoded.len() <= MAX_INLINE_BLOB_BYTES,
        "inline blob exceeds 16 MiB; use blob_chunks"
    );
    Ok(decoded.len())
}

fn typed_value_is_capsule(value: &Value) -> bool {
    match value.get("kind") {
        Some(Value::String(kind)) => matches!(
            kind.as_str(),
            "admitted_launch_capsule" | "persistent_session_capsule"
        ),
        Some(_) => true,
        None => false,
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPutEnvelope {
    #[allow(dead_code)]
    project_path: String,
    #[allow(dead_code)]
    #[serde(default)]
    staging_id: Option<String>,
    #[serde(default)]
    blobs: BoundedRawEntries,
    #[serde(default)]
    blob_chunks: BoundedRawEntries,
    #[serde(default)]
    objects: BoundedRawEntries,
}

#[derive(Debug, Default)]
struct BoundedRawEntries(Vec<Box<serde_json::value::RawValue>>);

impl<'de> serde::Deserialize<'de> for BoundedRawEntries {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        struct EntriesVisitor;
        impl<'de> serde::de::Visitor<'de> for EntriesVisitor {
            type Value = BoundedRawEntries;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("an array with at most 100000 raw upload entries")
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> std::result::Result<Self::Value, A::Error> {
                let mut entries = Vec::new();
                while let Some(entry) = seq.next_element::<Box<serde_json::value::RawValue>>()? {
                    if entries.len() >= MAX_OBJECT_PUT_ENTRIES {
                        return Err(serde::de::Error::custom(
                            "objects.put array exceeds 100000 entries",
                        ));
                    }
                    entries.push(entry);
                }
                Ok(BoundedRawEntries(entries))
            }
        }
        deserializer.deserialize_seq(EntriesVisitor)
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RawObjectEntry {
    value: Box<serde_json::value::RawValue>,
}

/// Validate the signed raw upload envelope before materializing its batch as
/// `serde_json::Value`s. Authentication is performed by the dispatcher before
/// this function is called, so the capsule discriminator sees the exact bytes
/// covered by the signature.
pub fn validate_raw_object_put_request(raw: &[u8]) -> anyhow::Result<()> {
    anyhow::ensure!(
        raw.len() <= MAX_OBJECT_PUT_BODY_BYTES,
        "objects.put body exceeds 100 MiB"
    );
    let envelope: RawPutEnvelope = serde_json::from_slice(raw)?;
    let entry_count =
        envelope.blobs.0.len() + envelope.blob_chunks.0.len() + envelope.objects.0.len();
    anyhow::ensure!(
        entry_count <= MAX_OBJECT_PUT_ENTRIES,
        "objects.put exceeds 100000 entries"
    );
    let mut aggregate = 0usize;
    for item in &envelope.objects.0 {
        let object: RawObjectEntry = serde_json::from_str(item.get())?;
        let bytes = object.value.get().as_bytes();
        anyhow::ensure!(
            bytes.len() <= MAX_RAW_OBJECT_BYTES,
            "objects.put object exceeds 32 MiB raw bytes"
        );
        let is_capsule = if bytes.iter().copied().find(|b| !b.is_ascii_whitespace()) == Some(b'{') {
            lillux::cas::json_object_is_capsule_kind(bytes)?
        } else {
            false
        };
        let ceiling = if is_capsule {
            MAX_CAPSULE_OBJECT_BYTES
        } else {
            MAX_RAW_OBJECT_BYTES
        };
        anyhow::ensure!(
            bytes.len() <= ceiling,
            "objects.put object exceeds {ceiling} raw bytes"
        );
        let value: Value = serde_json::from_str(object.value.get())?;
        let canonical_len = bounded_canonical_json_len(&value, ceiling)?;
        aggregate = accumulate_request_payload(aggregate, canonical_len)?;
    }
    for item in &envelope.blobs.0 {
        let blob: BlobEntry = serde_json::from_str(item.get())?;
        aggregate = accumulate_request_payload(aggregate, decode_inline_blob_bounded(&blob.data)?)?;
    }
    for item in &envelope.blob_chunks.0 {
        let chunk: BlobChunkEntry = serde_json::from_str(item.get())?;
        anyhow::ensure!(
            chunk.total_size <= MAX_BLOB_BYTES,
            "blob exceeds per-blob limit"
        );
        anyhow::ensure!(
            chunk.data.len() <= MAX_BLOB_CHUNK_BYTES.saturating_mul(2),
            "encoded blob chunk exceeds route-safe bound"
        );
        let decoded = base64::engine::general_purpose::STANDARD.decode(&chunk.data)?;
        anyhow::ensure!(
            decoded.len() <= MAX_BLOB_CHUNK_BYTES,
            "blob chunk exceeds 512 KiB"
        );
        aggregate = accumulate_request_payload(aggregate, decoded.len())?;
    }
    anyhow::ensure!(
        aggregate <= MAX_OBJECT_PUT_BODY_BYTES,
        "objects.put canonical payload exceeds 100 MiB"
    );
    Ok(())
}

/// Defensive limit checks for callers which construct the typed request
/// directly instead of entering through the signed raw route.
fn validate_typed_request_limits(req: &Request) -> anyhow::Result<()> {
    let count = req.blobs.len() + req.blob_chunks.len() + req.objects.len();
    anyhow::ensure!(
        count <= MAX_OBJECT_PUT_ENTRIES,
        "objects.put exceeds 100000 entries"
    );
    let mut aggregate = 0usize;
    for object in &req.objects {
        let capsule = typed_value_is_capsule(&object.value);
        let ceiling = if capsule {
            MAX_CAPSULE_OBJECT_BYTES
        } else {
            MAX_RAW_OBJECT_BYTES
        };
        let canonical_len = bounded_canonical_json_len(&object.value, ceiling)?;
        aggregate = accumulate_request_payload(aggregate, canonical_len)?;
    }
    for blob in &req.blobs {
        aggregate = accumulate_request_payload(aggregate, decode_inline_blob_bounded(&blob.data)?)?;
    }
    for chunk in &req.blob_chunks {
        anyhow::ensure!(
            chunk.total_size <= MAX_BLOB_BYTES,
            "blob exceeds per-blob limit"
        );
        anyhow::ensure!(
            chunk.data.len() <= MAX_BLOB_CHUNK_BYTES.saturating_mul(2),
            "encoded blob chunk exceeds route-safe bound"
        );
        let decoded = base64::engine::general_purpose::STANDARD.decode(&chunk.data)?;
        anyhow::ensure!(
            decoded.len() <= MAX_BLOB_CHUNK_BYTES,
            "blob chunk exceeds 512 KiB"
        );
        aggregate = accumulate_request_payload(aggregate, decoded.len())?;
    }
    anyhow::ensure!(
        aggregate <= MAX_OBJECT_PUT_BODY_BYTES,
        "objects.put canonical payload exceeds 100 MiB"
    );
    Ok(())
}

pub async fn handle(req: Request, ctx: HandlerContext, state: Arc<AppState>) -> Result<Value> {
    ctx.require_verified()
        .map_err(|error| anyhow::anyhow!(error))?;
    validate_typed_request_limits(&req)?;
    let authority = state
        .state_store
        .with_state_db(|db| db.pinned_authority())?;
    let cas_guard = authority.acquire_shared_guard()?;
    let cas = authority.cas_store()?;
    let _permit = state
        .write_barrier
        .acquire_with_timeout(ryeos_app::write_barrier::ONLINE_WRITE_PERMIT_TIMEOUT)
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

#[cfg(test)]
mod raw_ingress_controls {
    use super::*;

    #[test]
    fn raw_preflight_accepts_empty_upload_and_rejects_ambiguous_envelopes() {
        assert!(
            validate_raw_object_put_request(
                br#"{"project_path":"/project","blobs":[],"blob_chunks":[],"objects":[]}"#
            )
            .is_ok()
        );
        assert!(
            validate_raw_object_put_request(br#"{"project_path":"/project","extra":true}"#)
                .is_err()
        );
        assert!(
            validate_raw_object_put_request(
                br#"{"project_path":"/project","project_path":"/other"}"#
            )
            .is_err()
        );
    }

    #[test]
    fn raw_preflight_bounds_entry_count_before_value_materialization() {
        let make_body = |count: usize| {
            let mut raw = String::from(r#"{"project_path":"/project","blobs":["#);
            for index in 0..count {
                if index > 0 {
                    raw.push(',');
                }
                raw.push_str(r#"{"data":""}"#);
            }
            raw.push_str("]}");
            raw
        };
        assert!(
            validate_raw_object_put_request(make_body(MAX_OBJECT_PUT_ENTRIES).as_bytes()).is_ok()
        );
        assert!(
            validate_raw_object_put_request(make_body(MAX_OBJECT_PUT_ENTRIES + 1).as_bytes())
                .is_err()
        );
    }

    #[test]
    fn raw_preflight_bounds_combined_count_across_mixed_arrays() {
        fn entries(count: usize, entry: &str) -> String {
            let mut out = String::new();
            for index in 0..count {
                if index != 0 {
                    out.push(',');
                }
                out.push_str(entry);
            }
            out
        }
        let exact = format!(
            "{{\"project_path\":\"/p\",\"blobs\":[{}],\"blob_chunks\":[{}],\"objects\":[{}]}}",
            entries(33_334, r#"{"data":""}"#),
            entries(
                33_333,
                r#"{"hash":"h","total_size":1,"offset":0,"data":""}"#
            ),
            entries(33_333, r#"{"value":null}"#),
        );
        assert!(validate_raw_object_put_request(exact.as_bytes()).is_ok());

        let one_over = format!(
            "{{\"project_path\":\"/p\",\"blobs\":[{}],\"blob_chunks\":[{}],\"objects\":[{}]}}",
            entries(33_335, r#"{"data":""}"#),
            entries(
                33_333,
                r#"{"hash":"h","total_size":1,"offset":0,"data":""}"#
            ),
            entries(33_333, r#"{"value":null}"#),
        );
        assert!(validate_raw_object_put_request(one_over.as_bytes()).is_err());
    }

    #[test]
    fn raw_preflight_rejects_generic_object_over_32_mib_before_value_parse() {
        let value = format!(
            "{{\"payload\":\"{}\"}}",
            "x".repeat(MAX_RAW_OBJECT_BYTES + 1)
        );
        let raw = format!("{{\"project_path\":\"/project\",\"objects\":[{{\"value\":{value}}}]}}");
        assert!(validate_raw_object_put_request(raw.as_bytes()).is_err());
    }

    #[test]
    fn generic_object_raw_and_canonical_ceiling_accept_exact_and_reject_one_over() {
        let value_at = |raw_len: usize| format!("\"{}\"", "x".repeat(raw_len - 2));
        let make_body = |value: String| {
            format!("{{\"project_path\":\"/p\",\"objects\":[{{\"value\":{value}}}]}}")
        };
        assert!(
            validate_raw_object_put_request(make_body(value_at(MAX_RAW_OBJECT_BYTES)).as_bytes())
                .is_ok()
        );
        assert!(
            validate_raw_object_put_request(
                make_body(value_at(MAX_RAW_OBJECT_BYTES + 1)).as_bytes()
            )
            .is_err()
        );
    }

    #[test]
    fn generic_canonical_ceiling_accepts_exact_and_rejects_one_over_with_unicode_expansion() {
        let make_body = |canonical_len: usize| {
            let payload_len = canonical_len - 2;
            let astral_count = payload_len / 12;
            let ascii_count = payload_len % 12;
            let value = format!(
                "\"{}{}\"",
                "🧭".repeat(astral_count),
                "a".repeat(ascii_count)
            );
            format!("{{\"project_path\":\"/p\",\"objects\":[{{\"value\":{value}}}]}}")
        };
        let exact = make_body(MAX_RAW_OBJECT_BYTES);
        assert!(validate_raw_object_put_request(exact.as_bytes()).is_ok());
        let one_over = make_body(MAX_RAW_OBJECT_BYTES + 1);
        assert!(validate_raw_object_put_request(one_over.as_bytes()).is_err());
    }

    #[test]
    fn raw_preflight_rejects_recognized_capsule_over_16_mib_before_value_parse() {
        let value = format!(
            "{{\"kind\":\"admitted_launch_capsule\",\"payload\":\"{}\"}}",
            "x".repeat(MAX_CAPSULE_OBJECT_BYTES + 1)
        );
        let raw = format!("{{\"project_path\":\"/project\",\"objects\":[{{\"value\":{value}}}]}}");
        assert!(validate_raw_object_put_request(raw.as_bytes()).is_err());
    }

    #[test]
    fn capsule_raw_and_canonical_ceiling_accept_exact_and_reject_one_over() {
        let suffix = "\"}";
        let make_body = |kind_json: &str, raw_len: usize| {
            let prefix = format!("{{\"kind\":{kind_json},\"payload\":\"");
            let value = format!(
                "{}{}{}",
                prefix,
                "x".repeat(raw_len - prefix.len() - suffix.len()),
                suffix
            );
            format!("{{\"project_path\":\"/p\",\"objects\":[{{\"value\":{value}}}]}}")
        };
        for kind_json in [
            "\"admitted_launch_capsule\"",
            "\"persistent_session_capsule\"",
            "7",
        ] {
            assert!(
                validate_raw_object_put_request(
                    make_body(kind_json, MAX_CAPSULE_OBJECT_BYTES).as_bytes()
                )
                .is_ok()
            );
            assert!(
                validate_raw_object_put_request(
                    make_body(kind_json, MAX_CAPSULE_OBJECT_BYTES + 1).as_bytes()
                )
                .is_err()
            );
        }
    }

    #[test]
    fn capsule_canonical_ceiling_accepts_exact_and_rejects_one_over_with_unicode_expansion() {
        let suffix = "\"}";
        let prefix = r#"{"kind":"admitted_launch_capsule","payload":""#;
        let make_body = |canonical_len: usize| {
            let payload_len = canonical_len - prefix.len() - suffix.len();
            let astral_count = payload_len / 12;
            let ascii_count = payload_len % 12;
            let value = format!(
                "{}{}{}{}",
                prefix,
                "🧭".repeat(astral_count),
                "a".repeat(ascii_count),
                suffix
            );
            format!("{{\"project_path\":\"/p\",\"objects\":[{{\"value\":{value}}}]}}")
        };
        let exact = make_body(MAX_CAPSULE_OBJECT_BYTES);
        assert!(validate_raw_object_put_request(exact.as_bytes()).is_ok());
        let one_over = make_body(MAX_CAPSULE_OBJECT_BYTES + 1);
        assert!(validate_raw_object_put_request(one_over.as_bytes()).is_err());
    }

    #[test]
    fn raw_preflight_rejects_duplicate_top_level_capsule_discriminator() {
        let raw = br#"{"project_path":"/project","objects":[{"value":{"kind":"admitted_launch_capsule","kind":"ordinary","payload":1}}]}"#;
        assert!(validate_raw_object_put_request(raw).is_err());
    }

    #[test]
    fn raw_preflight_rejects_oversized_encoded_inline_blob_before_decode() {
        let encoded_max = MAX_INLINE_BLOB_BYTES.saturating_add(2) / 3 * 4;
        let raw = format!(
            "{{\"project_path\":\"/project\",\"blobs\":[{{\"data\":\"{}\"}}]}}",
            "A".repeat(encoded_max + 1)
        );
        assert!(validate_raw_object_put_request(raw.as_bytes()).is_err());
    }

    #[test]
    fn raw_preflight_rejects_oversized_chunk_and_declared_blob_before_decode() {
        let encoded_max = MAX_BLOB_CHUNK_BYTES * 2;
        let raw = format!(
            "{{\"project_path\":\"/project\",\"blob_chunks\":[{{\"hash\":\"h\",\"total_size\":{},\"offset\":0,\"data\":\"{}\"}}]}}",
            MAX_BLOB_BYTES + 1,
            "A".repeat(encoded_max + 1)
        );
        assert!(validate_raw_object_put_request(raw.as_bytes()).is_err());
    }

    #[test]
    fn decoded_inline_blob_boundary_is_exact() {
        let encoded_max = MAX_INLINE_BLOB_BYTES.saturating_add(2) / 3 * 4;
        let mut exact = "A".repeat(encoded_max - 2);
        exact.push_str("==");
        assert_eq!(
            decode_inline_blob_bounded(&exact).unwrap(),
            MAX_INLINE_BLOB_BYTES
        );

        let mut one_over = "A".repeat(encoded_max - 1);
        one_over.push('=');
        assert!(decode_inline_blob_bounded(&one_over).is_err());
    }

    #[test]
    fn decoded_blob_chunk_boundary_is_exact() {
        let make_body = |size: usize| {
            let encoded = base64::engine::general_purpose::STANDARD.encode(vec![0u8; size]);
            format!(
                "{{\"project_path\":\"/project\",\"blob_chunks\":[{{\"hash\":\"h\",\"total_size\":{},\"offset\":0,\"data\":\"{}\"}}]}}",
                size + 1,
                encoded
            )
        };
        assert!(
            validate_raw_object_put_request(make_body(MAX_BLOB_CHUNK_BYTES).as_bytes()).is_ok()
        );
        assert!(
            validate_raw_object_put_request(make_body(MAX_BLOB_CHUNK_BYTES + 1).as_bytes())
                .is_err()
        );
    }

    #[test]
    fn canonical_payload_aggregate_boundary_is_exact() {
        assert_eq!(
            accumulate_request_payload(MAX_OBJECT_PUT_BODY_BYTES - 1, 1).unwrap(),
            MAX_OBJECT_PUT_BODY_BYTES
        );
        assert!(accumulate_request_payload(MAX_OBJECT_PUT_BODY_BYTES - 1, 2).is_err());
    }

    #[test]
    fn raw_canonical_and_decoded_payload_aggregate_accepts_exact_and_rejects_one_over() {
        fn make_body(blob_bytes: usize) -> String {
            let mut raw = String::from(r#"{"project_path":"/p","objects":["#);
            for index in 0..3 {
                if index != 0 {
                    raw.push(',');
                }
                let payload_len = MAX_RAW_OBJECT_BYTES - 2;
                let astral_count = payload_len / 12;
                let ascii_count = payload_len % 12;
                let value = format!(
                    "\"{}{}\"",
                    "🧭".repeat(astral_count),
                    "a".repeat(ascii_count)
                );
                raw.push_str("{\"value\":");
                raw.push_str(&value);
                raw.push('}');
            }
            raw.push_str("],\"blobs\":[{\"data\":\"");
            raw.push_str(&base64::engine::general_purpose::STANDARD.encode(vec![0u8; blob_bytes]));
            raw.push_str("\"}]}");
            raw
        }
        let exact = make_body(4 * 1024 * 1024);
        assert!(exact.len() < MAX_OBJECT_PUT_BODY_BYTES);
        assert!(validate_raw_object_put_request(exact.as_bytes()).is_ok());
        let one_over = make_body(4 * 1024 * 1024 + 1);
        assert!(one_over.len() < MAX_OBJECT_PUT_BODY_BYTES);
        let error = validate_raw_object_put_request(one_over.as_bytes()).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("objects.put canonical payload exceeds 100 MiB")
        );
    }

    #[test]
    fn bounded_canonical_length_matches_cas_writer_for_escape_and_number_cases() {
        let fixtures = [
            serde_json::json!({"nul":"\u{0}","bmp":"λ","astral":"🧭"}),
            serde_json::json!({"nested":[true, false, null, {"quote":"\\\""}]}),
            serde_json::from_str::<Value>(r#"{"int":0,"float":0.0,"exp":1e+20}"#).unwrap(),
        ];
        for value in fixtures {
            assert_eq!(
                bounded_canonical_json_len(&value, MAX_OBJECT_PUT_BODY_BYTES).unwrap(),
                lillux::canonical_json(&value).unwrap().len()
            );
        }
    }

    #[test]
    fn typed_capsule_classifier_matches_pinned_raw_discriminator_rules() {
        for kind in ["admitted_launch_capsule", "persistent_session_capsule"] {
            assert!(typed_value_is_capsule(&serde_json::json!({"kind":kind})));
        }
        assert!(typed_value_is_capsule(&serde_json::json!({"kind":7})));
        assert!(typed_value_is_capsule(&serde_json::json!({"kind":null})));
        assert!(!typed_value_is_capsule(
            &serde_json::json!({"kind":"ordinary"})
        ));
        assert!(!typed_value_is_capsule(&serde_json::json!({"other":true})));
    }
}
