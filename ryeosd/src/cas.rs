use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::Result;
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(&mut out, "{byte:02x}");
    }
    out
}

fn shard_path(root: &Path, namespace: &str, hash: &str, ext: &str) -> PathBuf {
    root.join(namespace)
        .join(&hash[..2])
        .join(&hash[2..4])
        .join(format!("{hash}{ext}"))
}

/// Compute canonical JSON: sorted keys, compact separators.
fn canonical_json(value: &Value) -> String {
    fn write_canonical(val: &Value, out: &mut String) {
        match val {
            Value::Null => out.push_str("null"),
            Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Value::Number(n) => out.push_str(&n.to_string()),
            Value::String(s) => {
                out.push('"');
                for ch in s.chars() {
                    match ch {
                        '"' => out.push_str("\\\""),
                        '\\' => out.push_str("\\\\"),
                        '\n' => out.push_str("\\n"),
                        '\r' => out.push_str("\\r"),
                        '\t' => out.push_str("\\t"),
                        c if c < '\x20' => {
                            out.push_str(&format!("\\u{:04x}", c as u32));
                        }
                        c => out.push(c),
                    }
                }
                out.push('"');
            }
            Value::Array(arr) => {
                out.push('[');
                for (i, item) in arr.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    write_canonical(item, out);
                }
                out.push(']');
            }
            Value::Object(map) => {
                let mut keys: Vec<&String> = map.keys().collect();
                keys.sort();
                out.push('{');
                for (i, key) in keys.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    write_canonical(&Value::String((*key).clone()), out);
                    out.push(':');
                    write_canonical(&map[*key], out);
                }
                out.push('}');
            }
        }
    }

    let mut result = String::new();
    write_canonical(value, &mut result);
    result
}

fn sha256_hex(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hex_encode(&hasher.finalize())
}

/// Atomic write: write to temp file then rename.
fn atomic_write(dest: &Path, data: &[u8]) -> Result<()> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    let temp = dest.with_extension("tmp");
    let mut file = fs::File::create(&temp)?;
    file.write_all(data)?;
    file.sync_all()?;
    fs::rename(&temp, dest)?;
    Ok(())
}

// --- Store ---

pub struct CasStore {
    root: PathBuf,
}

impl CasStore {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn has_blob(&self, hash: &str) -> bool {
        shard_path(&self.root, "blobs", hash, "").exists()
    }

    pub fn has_object(&self, hash: &str) -> bool {
        shard_path(&self.root, "objects", hash, ".json").exists()
    }

    pub fn has(&self, hash: &str) -> bool {
        self.has_blob(hash) || self.has_object(hash)
    }

    pub fn get_blob(&self, hash: &str) -> Result<Option<Vec<u8>>> {
        let path = shard_path(&self.root, "blobs", hash, "");
        if !path.exists() {
            return Ok(None);
        }
        Ok(Some(fs::read(&path)?))
    }

    pub fn get_object(&self, hash: &str) -> Result<Option<Value>> {
        let path = shard_path(&self.root, "objects", hash, ".json");
        if !path.exists() {
            return Ok(None);
        }
        let data = fs::read(&path)?;
        let value: Value = serde_json::from_slice(&data)?;
        Ok(Some(value))
    }

    pub fn store_blob(&self, data: &[u8]) -> Result<String> {
        let hash = sha256_hex(data);
        let dest = shard_path(&self.root, "blobs", &hash, "");
        if dest.exists() {
            return Ok(hash);
        }
        atomic_write(&dest, data)?;
        Ok(hash)
    }

    pub fn store_object(&self, value: &Value) -> Result<String> {
        let json = canonical_json(value);
        let hash = sha256_hex(json.as_bytes());
        let dest = shard_path(&self.root, "objects", &hash, ".json");
        if dest.exists() {
            return Ok(hash);
        }
        atomic_write(&dest, json.as_bytes())?;
        Ok(hash)
    }
}

// --- Typed CAS objects ---

/// A single ingested item stored in CAS.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ItemSource {
    pub item_ref: String,
    pub content_blob_hash: String,
    pub integrity: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature_info: Option<Value>,
}

impl ItemSource {
    pub fn to_json(&self) -> Value {
        let mut v = json!({
            "kind": "item_source",
            "item_ref": self.item_ref,
            "content_blob_hash": self.content_blob_hash,
            "integrity": self.integrity,
        });
        if let Some(ref sig) = self.signature_info {
            v.as_object_mut()
                .unwrap()
                .insert("signature_info".into(), sig.clone());
        }
        v
    }

    pub fn from_json(value: &Value) -> Result<Self> {
        Ok(Self {
            item_ref: value
                .get("item_ref")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            content_blob_hash: value
                .get("content_blob_hash")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            integrity: value
                .get("integrity")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            signature_info: value.get("signature_info").cloned(),
        })
    }

    pub fn child_hashes(&self) -> Vec<String> {
        vec![self.content_blob_hash.clone()]
    }
}

/// Maps relative paths to item source object hashes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceManifest {
    pub items: HashMap<String, String>,
}

impl SourceManifest {
    pub fn to_json(&self) -> Value {
        json!({
            "kind": "source_manifest",
            "items": self.items,
        })
    }

    pub fn from_json(value: &Value) -> Result<Self> {
        let items = value
            .get("items")
            .and_then(|v| v.as_object())
            .map(|m| {
                m.iter()
                    .map(|(k, v)| (k.clone(), v.as_str().unwrap_or("").to_string()))
                    .collect()
            })
            .unwrap_or_default();
        Ok(Self { items })
    }

    pub fn child_hashes(&self) -> Vec<String> {
        self.items.values().cloned().collect()
    }
}

/// A point-in-time project snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectSnapshot {
    pub project_manifest_hash: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_manifest_hash: Option<String>,
    pub parent_hashes: Vec<String>,
    pub created_at: String,
    pub push_type: String,
}

impl ProjectSnapshot {
    pub fn to_json(&self) -> Value {
        let mut v = json!({
            "kind": "project_snapshot",
            "schema": 2,
            "project_manifest_hash": self.project_manifest_hash,
            "parent_hashes": self.parent_hashes,
            "created_at": self.created_at,
            "source": self.push_type,
        });
        if let Some(ref umh) = self.user_manifest_hash {
            v.as_object_mut()
                .unwrap()
                .insert("user_manifest_hash".into(), json!(umh));
        }
        v
    }

    pub fn from_json(value: &Value) -> Result<Self> {
        Ok(Self {
            project_manifest_hash: value
                .get("project_manifest_hash")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            user_manifest_hash: value
                .get("user_manifest_hash")
                .and_then(|v| v.as_str())
                .map(String::from),
            parent_hashes: value
                .get("parent_hashes")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default(),
            created_at: value
                .get("created_at")
                .or_else(|| value.get("timestamp"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            push_type: value
                .get("source")
                .and_then(|v| v.as_str())
                .unwrap_or("manual")
                .to_string(),
        })
    }

    pub fn child_hashes(&self) -> Vec<String> {
        let mut hashes = vec![self.project_manifest_hash.clone()];
        if let Some(ref umh) = self.user_manifest_hash {
            hashes.push(umh.clone());
        }
        hashes.extend(self.parent_hashes.iter().cloned());
        hashes
    }
}

/// Kind-aware child hash extraction from a raw JSON CAS object.
///
/// Used by GC mark phase to walk the object graph.
/// Delegates to typed struct `child_hashes()` for known kinds.
pub fn extract_child_hashes(obj: &Value) -> Vec<String> {
    match obj.get("kind").and_then(|k| k.as_str()) {
        Some("project_snapshot") => match ProjectSnapshot::from_json(obj) {
            Ok(snap) => snap.child_hashes(),
            Err(_) => Vec::new(),
        },
        Some("source_manifest") => match SourceManifest::from_json(obj) {
            Ok(manifest) => manifest.child_hashes(),
            Err(_) => Vec::new(),
        },
        Some("item_source") => match ItemSource::from_json(obj) {
            Ok(item) => item.child_hashes(),
            Err(_) => Vec::new(),
        },
        Some("execution_snapshot") => {
            match crate::execution::snapshot::ExecutionSnapshot::from_json(obj) {
                Ok(snap) => {
                    let mut hashes = vec![snap.project_manifest_hash];
                    if let Some(umh) = snap.user_manifest_hash {
                        hashes.push(umh);
                    }
                    hashes
                }
                Err(_) => Vec::new(),
            }
        }
        Some("runtime_outputs_bundle") => {
            match crate::execution::snapshot::RuntimeOutputsBundle::from_json(obj) {
                Ok(bundle) => {
                    let mut hashes = vec![bundle.execution_snapshot_hash];
                    if let Some(omh) = bundle.output_manifest_hash {
                        hashes.push(omh);
                    }
                    for a in &bundle.artifacts {
                        hashes.push(a.blob_hash.clone());
                    }
                    hashes
                }
                Err(_) => Vec::new(),
            }
        }
        _ => Vec::new(),
    }
}

// --- Ingest / materialize ---

/// Result of ingesting a single item into CAS.
#[derive(Debug, Clone)]
pub struct IngestResult {
    pub blob_hash: String,
    pub object_hash: String,
    pub integrity: String,
}

impl CasStore {
    /// Create a CAS store scoped to a specific principal.
    pub fn for_principal(cas_root: &Path, principal_fp: &str) -> Self {
        let root = cas_root
            .join("principals")
            .join(principal_fp)
            .join("objects");
        Self { root }
    }

    /// Ingest a single file into CAS.
    ///
    /// Stores the raw file content as a blob, extracts signature info,
    /// and creates an `ItemSource` object.
    pub fn ingest_item(&self, item_ref: &str, file_path: &Path) -> Result<IngestResult> {
        let bytes = fs::read(file_path)?;
        let blob_hash = self.store_blob(&bytes)?;
        let integrity = sha256_hex(&bytes);

        let signature_info = parse_signature_info_from_bytes(&bytes);

        let source = ItemSource {
            item_ref: item_ref.to_string(),
            content_blob_hash: blob_hash.clone(),
            integrity: integrity.clone(),
            signature_info,
        };
        let object_hash = self.store_object(&source.to_json())?;

        Ok(IngestResult {
            blob_hash,
            object_hash,
            integrity,
        })
    }

    /// Ingest all files under a directory into CAS.
    ///
    /// Walks the tree recursively, skipping `state/` subdirectories.
    /// Returns a map of relative path → item source object hash.
    pub fn ingest_directory(&self, base_path: &Path) -> Result<HashMap<String, String>> {
        let mut items = HashMap::new();
        ingest_walk(self, base_path, base_path, &mut items)?;
        Ok(items)
    }

    /// Materialize an item from CAS back to the filesystem.
    ///
    /// Loads the `ItemSource` object, reads its content blob, and
    /// writes it to `target_path` atomically.
    pub fn materialize_item(&self, object_hash: &str, target_path: &Path) -> Result<()> {
        let obj = self
            .get_object(object_hash)?
            .ok_or_else(|| anyhow::anyhow!("item source object {object_hash} not found"))?;

        let blob_hash = obj
            .get("content_blob_hash")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("missing content_blob_hash in item source"))?;

        let data = self
            .get_blob(blob_hash)?
            .ok_or_else(|| anyhow::anyhow!("blob {blob_hash} not found"))?;

        atomic_write(target_path, &data)?;
        Ok(())
    }
}

fn ingest_walk(
    store: &CasStore,
    root: &Path,
    dir: &Path,
    items: &mut HashMap<String, String>,
) -> Result<()> {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return Ok(()),
    };
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let rel = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .to_string();

        // Skip state/ subdirectory
        if rel.starts_with("state/") || rel == "state" {
            continue;
        }

        if path.is_dir() {
            ingest_walk(store, root, &path, items)?;
        } else if path.is_file() {
            let result = store.ingest_item(&rel, &path)?;
            items.insert(rel, result.object_hash);
        }
    }
    Ok(())
}

/// Parse signature info from file bytes (first line check).
fn parse_signature_info_from_bytes(bytes: &[u8]) -> Option<Value> {
    let content = std::str::from_utf8(bytes).ok()?;
    let first_line = content.lines().next()?;

    // Try hash-prefix: # rye:signed:...
    let remainder = if let Some(r) = first_line.strip_prefix("# rye:signed:") {
        r
    } else if let Some(inner) = first_line.strip_prefix("<!-- rye:signed:") {
        inner.strip_suffix("-->")?.trim()
    } else {
        return None;
    };

    // Parse: TIMESTAMP:HASH:SIG:FP (rsplit from right, timestamp may have colons)
    let parts: Vec<&str> = remainder.rsplitn(4, ':').collect();
    if parts.len() != 4 {
        return None;
    }

    Some(json!({
        "signer": parts[0],
        "signature": parts[1],
        "hash": parts[2],
        "timestamp": parts[3],
    }))
}

// --- Manifest validation ---

/// Validate that all objects and blobs referenced by a manifest exist in CAS.
///
/// Walks `source_manifest` items → `item_source` objects → content blobs.
/// Returns Ok(()) if complete, Err with missing hashes on failure.
pub fn validate_manifest(cas: &CasStore, manifest_hash: &str) -> Result<(), String> {
    let manifest = cas
        .get_object(manifest_hash)
        .map_err(|e| format!("failed to read manifest: {e}"))?
        .ok_or_else(|| format!("manifest {manifest_hash} not found"))?;

    let items = match manifest.get("items").and_then(|v| v.as_object()) {
        Some(m) => m,
        None => return Ok(()), // Not a source_manifest, skip deep validation
    };

    let mut missing = Vec::new();

    for (rel_path, item_hash_val) in items {
        let item_hash = match item_hash_val.as_str() {
            Some(h) => h,
            None => continue,
        };

        match cas.get_object(item_hash) {
            Ok(Some(item_obj)) => {
                // Verify the content blob exists
                if let Some(blob_hash) = item_obj.get("content_blob_hash").and_then(|v| v.as_str())
                {
                    if !cas.has_blob(blob_hash) {
                        missing.push(format!("blob {} (from {})", blob_hash, rel_path));
                    }
                }
            }
            Ok(None) => {
                missing.push(format!("item_source {} (for {})", item_hash, rel_path));
            }
            Err(e) => {
                missing.push(format!("error reading {}: {}", item_hash, e));
            }
        }
    }

    if missing.is_empty() {
        Ok(())
    } else {
        Err(format!("missing CAS objects: {}", missing.join(", ")))
    }
}

// --- API types ---

#[derive(Debug, Deserialize)]
pub struct HasObjectsRequest {
    pub hashes: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct HasObjectsResponse {
    pub present: Vec<String>,
    pub missing: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct ObjectEntry {
    pub hash: String,
    pub kind: String,
    pub data: String,
}

#[derive(Debug, Deserialize)]
pub struct PutObjectsRequest {
    pub entries: Vec<ObjectEntry>,
}

#[derive(Debug, Serialize)]
pub struct PutObjectsResponse {
    pub stored: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<PutError>,
}

#[derive(Debug, Serialize)]
pub struct PutError {
    pub hash: String,
    pub error: String,
}

#[derive(Debug, Deserialize)]
pub struct GetObjectsRequest {
    pub hashes: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct GetObjectsEntry {
    pub hash: String,
    pub kind: String,
    pub data: String,
}

#[derive(Debug, Serialize)]
pub struct GetObjectsResponse {
    pub entries: Vec<GetObjectsEntry>,
}

// --- Handlers ---

impl CasStore {
    pub fn handle_has(&self, req: &HasObjectsRequest) -> HasObjectsResponse {
        let mut present = Vec::new();
        let mut missing = Vec::new();
        for hash in &req.hashes {
            if self.has(hash) {
                present.push(hash.clone());
            } else {
                missing.push(hash.clone());
            }
        }
        HasObjectsResponse { present, missing }
    }

    pub fn handle_put(&self, req: &PutObjectsRequest) -> PutObjectsResponse {
        let b64 = base64::engine::general_purpose::STANDARD;
        let mut stored = Vec::new();
        let mut errors = Vec::new();

        for entry in &req.entries {
            let raw = match b64.decode(&entry.data) {
                Ok(data) => data,
                Err(err) => {
                    errors.push(PutError {
                        hash: entry.hash.clone(),
                        error: format!("base64 decode error: {err}"),
                    });
                    continue;
                }
            };

            let actual_hash = if entry.kind == "blob" {
                sha256_hex(&raw)
            } else {
                match serde_json::from_slice::<Value>(&raw) {
                    Ok(val) => {
                        let json = canonical_json(&val);
                        sha256_hex(json.as_bytes())
                    }
                    Err(err) => {
                        errors.push(PutError {
                            hash: entry.hash.clone(),
                            error: format!("invalid JSON object: {err}"),
                        });
                        continue;
                    }
                }
            };

            if actual_hash != entry.hash {
                errors.push(PutError {
                    hash: entry.hash.clone(),
                    error: format!(
                        "hash mismatch: claimed {}… got {}…",
                        &entry.hash[..entry.hash.len().min(16)],
                        &actual_hash[..actual_hash.len().min(16)]
                    ),
                });
                continue;
            }

            let result = if entry.kind == "blob" {
                self.store_blob(&raw)
            } else {
                match serde_json::from_slice::<Value>(&raw) {
                    Ok(val) => self.store_object(&val),
                    Err(err) => {
                        errors.push(PutError {
                            hash: entry.hash.clone(),
                            error: format!("invalid JSON: {err}"),
                        });
                        continue;
                    }
                }
            };

            match result {
                Ok(_) => stored.push(entry.hash.clone()),
                Err(err) => {
                    errors.push(PutError {
                        hash: entry.hash.clone(),
                        error: err.to_string(),
                    });
                }
            }
        }

        PutObjectsResponse { stored, errors }
    }

    pub fn handle_get(&self, req: &GetObjectsRequest) -> GetObjectsResponse {
        let b64 = base64::engine::general_purpose::STANDARD;
        let mut entries = Vec::new();

        for hash in &req.hashes {
            if let Ok(Some(data)) = self.get_blob(hash) {
                entries.push(GetObjectsEntry {
                    hash: hash.clone(),
                    kind: "blob".to_string(),
                    data: b64.encode(&data),
                });
                continue;
            }

            if let Ok(Some(val)) = self.get_object(hash) {
                let json = canonical_json(&val);
                entries.push(GetObjectsEntry {
                    hash: hash.clone(),
                    kind: "object".to_string(),
                    data: b64.encode(json.as_bytes()),
                });
            }
        }

        GetObjectsResponse { entries }
    }
}
