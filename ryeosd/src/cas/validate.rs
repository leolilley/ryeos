use anyhow::Result;

use super::CasStore;

pub fn validate_manifest(cas: &CasStore, manifest_hash: &str) -> Result<(), String> {
    let manifest = cas
        .get_object(manifest_hash)
        .map_err(|e| format!("failed to read manifest: {e}"))?
        .ok_or_else(|| format!("manifest {manifest_hash} not found"))?;

    let items = manifest
        .get("item_source_hashes")
        .and_then(|v| v.as_object())
        .ok_or_else(|| "manifest missing item_source_hashes".to_string())?;

    let mut missing = Vec::new();

    for (rel_path, item_hash_val) in items {
        let item_hash = item_hash_val
            .as_str()
            .ok_or_else(|| format!("item_source_hashes[{rel_path}] is not a string"))?;

        match cas.get_object(item_hash) {
            Ok(Some(item_obj)) => {
                let blob_hash = item_obj
                    .get("content_blob_hash")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        format!(
                            "item_source {item_hash} (for {rel_path}) missing content_blob_hash"
                        )
                    })?;
                if !cas.has_blob(blob_hash) {
                    missing.push(format!("blob {blob_hash} (from {rel_path})"));
                }
            }
            Ok(None) => {
                missing.push(format!("item_source {item_hash} (for {rel_path})"));
            }
            Err(e) => {
                missing.push(format!("error reading {item_hash}: {e}"));
            }
        }
    }

    if missing.is_empty() {
        Ok(())
    } else {
        Err(format!("missing CAS objects: {}", missing.join(", ")))
    }
}
