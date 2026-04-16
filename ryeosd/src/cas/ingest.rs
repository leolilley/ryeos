use std::collections::HashMap;
use std::fs;
use std::path::Path;

use anyhow::Result;
use serde_json::{json, Value};

use super::types::ItemSource;
use super::CasStore;

pub struct IngestResult {
    pub blob_hash: String,
    pub object_hash: String,
    pub integrity: String,
}

pub fn ingest_item(store: &CasStore, item_ref: &str, file_path: &Path) -> Result<IngestResult> {
    let bytes = fs::read(file_path)?;
    let blob_hash = store.store_blob(&bytes)?;
    let integrity = super::sha256_hex(&bytes);

    let signature_info = parse_signature_info_from_bytes(&bytes);

    let source = ItemSource {
        item_ref: item_ref.to_string(),
        content_blob_hash: blob_hash.clone(),
        integrity: integrity.clone(),
        signature_info,
    };
    let object_hash = store.store_object(&source.to_json())?;

    Ok(IngestResult {
        blob_hash,
        object_hash,
        integrity,
    })
}

pub fn ingest_directory(store: &CasStore, base_path: &Path) -> Result<HashMap<String, String>> {
    let mut items = HashMap::new();
    ingest_walk(store, base_path, base_path, &mut items)?;
    Ok(items)
}

pub fn materialize_item(store: &CasStore, object_hash: &str, target_path: &Path) -> Result<()> {
    let obj = store
        .get_object(object_hash)?
        .ok_or_else(|| anyhow::anyhow!("item source object {object_hash} not found"))?;

    let blob_hash = obj
        .get("content_blob_hash")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing content_blob_hash in item source"))?;

    let data = store
        .get_blob(blob_hash)?
        .ok_or_else(|| anyhow::anyhow!("blob {blob_hash} not found"))?;

    super::atomic_write(target_path, &data)?;
    Ok(())
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

        if rel.starts_with("state/") || rel == "state" {
            continue;
        }

        if path.is_dir() {
            ingest_walk(store, root, &path, items)?;
        } else if path.is_file() {
            let result = ingest_item(store, &rel, &path)?;
            items.insert(rel, result.object_hash);
        }
    }
    Ok(())
}

fn parse_signature_info_from_bytes(bytes: &[u8]) -> Option<Value> {
    let content = std::str::from_utf8(bytes).ok()?;
    let first_line = content.lines().next()?;

    let remainder = if let Some(r) = first_line.strip_prefix("# rye:signed:") {
        r
    } else if let Some(inner) = first_line.strip_prefix("<!-- rye:signed:") {
        inner.strip_suffix("-->")?.trim()
    } else {
        return None;
    };

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
