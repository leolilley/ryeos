use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::Result;
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::Value;
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
