use std::collections::HashMap;
use std::path::Path;

use crate::error::EngineError;
use crate::trust::TrustStore;

use super::{load_and_verify_kind_schema, KindSchema};

const KIND_SCHEMA_SUFFIX: &str = ".kind-schema.yaml";

pub(super) fn load_schemas_from_dir(
    kinds_root: &Path,
    schemas: &mut HashMap<String, KindSchema>,
    schema_content_hashes: &mut HashMap<String, String>,
    fingerprint_data: &mut Vec<u8>,
    trust_store: &TrustStore,
) -> Result<(), EngineError> {
    let dir_entries = match std::fs::read_dir(kinds_root) {
        Ok(d) => d,
        Err(e) => {
            return Err(EngineError::SchemaLoaderError {
                reason: format!("cannot read kinds dir {}: {e}", kinds_root.display()),
            });
        }
    };

    let mut kind_dirs: Vec<_> = dir_entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    kind_dirs.sort();

    for kind_dir in kind_dirs {
        let kind_name = match kind_dir.file_name().and_then(|n| n.to_str()) {
            Some(n) => n.to_owned(),
            None => continue,
        };

        let yaml_entries = match std::fs::read_dir(&kind_dir) {
            Ok(d) => d,
            Err(_) => continue,
        };

        let mut schema_files: Vec<_> = yaml_entries
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| is_loadable_schema_file(p))
            .collect();
        schema_files.sort();

        for yaml_path in schema_files {
            // First-found wins. Shadowing is checked before verification so a
            // schema from an earlier search root owns the kind completely.
            if schemas.contains_key(&kind_name) {
                tracing::debug!(
                    kind = %kind_name,
                    path = %yaml_path.display(),
                    "skipped shadowed kind schema (earlier root claimed this kind)"
                );
                continue;
            }

            let (parsed, content_hash) = load_and_verify_kind_schema(&yaml_path, trust_store)?;
            schemas.insert(kind_name.clone(), parsed);
            schema_content_hashes.insert(kind_name.clone(), content_hash);
            if let Ok(content) = std::fs::read(&yaml_path) {
                fingerprint_data.extend_from_slice(&content);
            }
            tracing::debug!(kind = %kind_name, path = %yaml_path.display(), "loaded kind schema");
        }
    }

    Ok(())
}

fn is_loadable_schema_file(path: &Path) -> bool {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    name.ends_with(KIND_SCHEMA_SUFFIX) && !name.starts_with('_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_filename_filter_accepts_only_public_kind_schema_files() {
        assert!(is_loadable_schema_file(Path::new("tool.kind-schema.yaml")));
        assert!(!is_loadable_schema_file(Path::new(
            "_tool.kind-schema.yaml"
        )));
        assert!(!is_loadable_schema_file(Path::new("tool.yaml")));
        assert!(!is_loadable_schema_file(Path::new("tool.kind-schema.yml")));
    }
}
