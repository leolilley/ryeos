use std::collections::HashMap;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use serde_json::{json, Value};

use ryeos_state::objects::ItemSource;

use ryeos_app::ignore::IgnoreMatcher;

pub struct IngestResult {
    pub blob_hash: String,
    pub object_hash: String,
    pub integrity: String,
}

pub fn ingest_item(
    authority: &ryeos_state::PinnedStateAuthority,
    guard: &ryeos_state::CasMutationGuard,
    item_ref: &str,
    file_path: &Path,
) -> Result<IngestResult> {
    authority.ensure_guard(guard)?;
    let cas = authority.cas_store()?;
    let bytes = fs::read(file_path)?;
    let blob_hash = cas.store_blob(&bytes)?;
    // CAS blob identity and ItemSource integrity are the same SHA-256 of the
    // source bytes. Reuse the verified address instead of hashing every file a
    // second time during live-project capture.
    let integrity = blob_hash.clone();

    let signature_info = parse_signature_info_from_bytes(&bytes);

    // Detect Unix exec bit on the source file.
    let mode = fs::metadata(file_path)
        .ok()
        .map(|m| {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                m.permissions().mode() & 0o7777
            }
            #[cfg(not(unix))]
            None
        })
        .filter(|m| m & 0o111 != 0);

    let source = ItemSource {
        item_ref: item_ref.to_string(),
        content_blob_hash: blob_hash.clone(),
        integrity: integrity.clone(),
        signature_info,
        mode,
    };
    let object_hash = cas.store_object(&source.to_value())?;

    Ok(IngestResult {
        blob_hash,
        object_hash,
        integrity,
    })
}

/// Ingest a directory into CAS, applying the ignore matcher to skip
/// excluded paths (`.git/`, `node_modules/`, etc.).
pub fn ingest_directory(
    authority: &ryeos_state::PinnedStateAuthority,
    guard: &ryeos_state::CasMutationGuard,
    base_path: &Path,
    ignore: &IgnoreMatcher,
) -> Result<HashMap<String, String>> {
    let mut items = HashMap::new();
    ingest_walk(authority, guard, base_path, base_path, &mut items, ignore)?;
    Ok(items)
}

pub fn materialize_item(
    authority: &ryeos_state::PinnedStateAuthority,
    guard: &ryeos_state::CasMutationGuard,
    object_hash: &str,
    target_path: &Path,
) -> Result<()> {
    authority.ensure_guard(guard)?;
    let cas = authority.cas_store()?;
    let obj = cas
        .get_object(object_hash)?
        .ok_or_else(|| anyhow::anyhow!("item source object {object_hash} not found"))?;

    let blob_hash = obj
        .get("content_blob_hash")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing content_blob_hash in item source"))?;

    let data = cas
        .get_blob(blob_hash)?
        .ok_or_else(|| anyhow::anyhow!("blob {blob_hash} not found"))?;

    lillux::atomic_write(target_path, &data)?;
    Ok(())
}

fn ingest_walk(
    authority: &ryeos_state::PinnedStateAuthority,
    guard: &ryeos_state::CasMutationGuard,
    root: &Path,
    dir: &Path,
    items: &mut HashMap<String, String>,
    ignore: &IgnoreMatcher,
) -> Result<()> {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return Ok(()),
    };
    for entry in entries {
        let entry = entry?;
        let file_type = entry.file_type()?;
        // Project snapshots capture source bytes, not filesystem topology.
        // Never follow a symlink out of (or recursively back into) the live
        // project. Virtualenv interpreter links are intentionally omitted.
        if file_type.is_symlink() {
            continue;
        }
        let path = entry.path();
        let relative = path.strip_prefix(root).with_context(|| {
            format!(
                "ingest path '{}' escaped project root '{}'",
                path.display(),
                root.display()
            )
        })?;
        let rel = relative
            .to_str()
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "ingest project-relative path '{}' is not valid UTF-8",
                    relative.display()
                )
            })?
            .replace('\\', "/");

        // Always skip the state/ directory (internal daemon state)
        if rel.starts_with("state/") || rel == "state" {
            continue;
        }

        // Apply shared ignore rules
        if ignore.is_ignored(&rel) {
            continue;
        }

        if file_type.is_dir() {
            ingest_walk(authority, guard, root, &path, items, ignore)?;
        } else if file_type.is_file() {
            // The caller holds the global CAS write permit and mutation guard
            // for the complete walk. GC therefore cannot observe or sweep the
            // descendants before the final SourceManifest is durably staged.
            // Staging every intermediate hash would rewrite a growing recovery
            // record once per blob and object (quadratic metadata I/O); the
            // manifest's verified closure protects all of them after capture.
            let result = ingest_item(authority, guard, &rel, &path)?;
            items.insert(rel, result.object_hash);
        }
    }
    Ok(())
}

fn parse_signature_info_from_bytes(bytes: &[u8]) -> Option<Value> {
    let content = std::str::from_utf8(bytes).ok()?;
    let first_line = content.lines().next()?;

    let remainder = if let Some(r) = first_line.strip_prefix("# ryeos:signed:") {
        r
    } else if let Some(inner) = first_line.strip_prefix("<!-- ryeos:signed:") {
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
