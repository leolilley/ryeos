use base64::Engine;
use serde::{Deserialize, Serialize};

use super::CasStore;

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

pub fn handle_has(store: &CasStore, req: &HasObjectsRequest) -> HasObjectsResponse {
    let mut present = Vec::new();
    let mut missing = Vec::new();
    for hash in &req.hashes {
        if store.has(hash) {
            present.push(hash.clone());
        } else {
            missing.push(hash.clone());
        }
    }
    HasObjectsResponse { present, missing }
}

pub fn handle_put(store: &CasStore, req: &PutObjectsRequest) -> PutObjectsResponse {
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
            super::sha256_hex(&raw)
        } else {
            match serde_json::from_slice::<serde_json::Value>(&raw) {
                Ok(val) => {
                    let json = super::canonical_json(&val);
                    super::sha256_hex(json.as_bytes())
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
            store.store_blob(&raw)
        } else {
            match serde_json::from_slice::<serde_json::Value>(&raw) {
                Ok(val) => store.store_object(&val),
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

pub fn handle_get(store: &CasStore, req: &GetObjectsRequest) -> GetObjectsResponse {
    let b64 = base64::engine::general_purpose::STANDARD;
    let mut entries = Vec::new();

    for hash in &req.hashes {
        if let Ok(Some(data)) = store.get_blob(hash) {
            entries.push(GetObjectsEntry {
                hash: hash.clone(),
                kind: "blob".to_string(),
                data: b64.encode(&data),
            });
            continue;
        }

        if let Ok(Some(val)) = store.get_object(hash) {
            let json = super::canonical_json(&val);
            entries.push(GetObjectsEntry {
                hash: hash.clone(),
                kind: "object".to_string(),
                data: b64.encode(json.as_bytes()),
            });
        }
    }

    GetObjectsResponse { entries }
}
