//! Executable-member identity in an exact typed runtime manifest.
//!
//! This validates manifest data only. Actual payload verification, admitted
//! launch, and independent qualification remain duties of their existing owners.

use anyhow::{Context as _, Result, bail};
use serde_json::Value;

use crate::objects::{
    EXTERNAL_CONTENT_MANIFEST_KIND, EXTERNAL_LARGE_CONTENT_MANIFEST_KIND,
    ExternalContentManifestEntryKind, ExternalContentManifestObject,
    ExternalLargeContentManifestObject, validate_canonical_project_relative_path,
};

pub fn exact_runtime_member_hash(manifest: &Value, relative_path: &str) -> Result<String> {
    validate_canonical_project_relative_path(relative_path)?;
    let (kind, mode, size, digest) = match manifest.get("kind").and_then(Value::as_str) {
        Some(EXTERNAL_CONTENT_MANIFEST_KIND) => {
            let manifest = ExternalContentManifestObject::from_value(manifest)?;
            let entry = manifest
                .entries
                .into_iter()
                .find(|entry| entry.path == relative_path)
                .context("runtime has no selected executable member")?;
            (entry.kind, entry.mode, entry.size, entry.blob_hash)
        }
        Some(EXTERNAL_LARGE_CONTENT_MANIFEST_KIND) => {
            let manifest = ExternalLargeContentManifestObject::from_value(manifest)?;
            let entry = manifest
                .entries
                .into_iter()
                .find(|entry| entry.path == relative_path)
                .context("runtime has no selected executable member")?;
            // A chunked manifest retains the full-file hash separately from
            // its chunks; an ordinary manifest blob is itself the full file.
            (
                entry.kind,
                entry.mode,
                entry.size,
                entry.file_sha256.or(entry.blob_hash),
            )
        }
        _ => bail!("runtime has no supported exact manifest"),
    };
    if kind != ExternalContentManifestEntryKind::File
        || mode != Some(0o755)
        || size.unwrap_or(0) == 0
    {
        bail!("runtime member is not a nonempty executable regular file");
    }
    digest.context("runtime member has no exact file digest")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::objects::{EXTERNAL_CONTENT_TREE_SCHEMA, EXTERNAL_LARGE_CONTENT_SCHEMA};

    #[test]
    fn selected_member_requires_a_nonempty_regular_executable() {
        for (kind, schema) in [
            (EXTERNAL_CONTENT_MANIFEST_KIND, EXTERNAL_CONTENT_TREE_SCHEMA),
            (
                EXTERNAL_LARGE_CONTENT_MANIFEST_KIND,
                EXTERNAL_LARGE_CONTENT_SCHEMA,
            ),
        ] {
            let manifest = serde_json::json!({
                "kind":kind, "schema":schema, "entry_count":2, "total_bytes":4,
                "entries":[
                    {"path":"bin","kind":"dir"},
                    {"path":"bin/runner","kind":"file","mode":493,"blob_hash":"a".repeat(64),"size":4}
                ],
            });
            assert_eq!(
                exact_runtime_member_hash(&manifest, "bin/runner").unwrap(),
                "a".repeat(64)
            );
            for path in ["bin/missing", "bin", "../bin/runner"] {
                assert!(exact_runtime_member_hash(&manifest, path).is_err());
            }
            let mut changed = manifest.clone();
            changed["entries"][1]["mode"] = serde_json::json!(420);
            assert!(exact_runtime_member_hash(&changed, "bin/runner").is_err());
            let mut changed = manifest;
            changed["entries"][1]["size"] = serde_json::json!(0);
            changed["total_bytes"] = serde_json::json!(0);
            assert!(exact_runtime_member_hash(&changed, "bin/runner").is_err());
        }
    }
}
