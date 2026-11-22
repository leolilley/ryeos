//! Measuring an image seed tree.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result};
use lillux::DirectoryTraversalBudget;

const SEED_BUDGET: DirectoryTraversalBudget = DirectoryTraversalBudget::new(262_144, 32);

/// Digest of an image seed tree: every regular file by relative path and
/// content. Links and special files fail the measurement, so a seed that
/// measures is a seed made only of the bytes it names.
pub fn seed_tree_digest(root: &Path) -> Result<String> {
    let mut entries: BTreeMap<String, String> = BTreeMap::new();
    let present = lillux::visit_regular_files_no_follow_bounded(
        root,
        SEED_BUDGET,
        |_, _| Ok(false),
        |relative, file| {
            let length = file.metadata()?.len();
            let (digest, _) = lillux::digest_open_regular_file_stable_exact(&file, length)?;
            entries.insert(relative.to_string_lossy().into_owned(), digest);
            Ok(())
        },
    )
    .with_context(|| format!("measure image seed {}", root.display()))?;
    if !present {
        anyhow::bail!("image seed {} is absent", root.display());
    }
    Ok(lillux::sha256_hex(
        lillux::canonical_json(&serde_json::json!(entries))?.as_bytes(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_digest_follows_names_and_bytes() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("core")).unwrap();
        std::fs::write(root.path().join("core/a"), b"one").unwrap();
        let first = seed_tree_digest(root.path()).unwrap();
        assert_eq!(first, seed_tree_digest(root.path()).unwrap());
        std::fs::write(root.path().join("core/a"), b"two").unwrap();
        assert_ne!(first, seed_tree_digest(root.path()).unwrap());
    }

    #[test]
    fn an_absent_seed_does_not_measure() {
        let root = tempfile::tempdir().unwrap();
        assert!(seed_tree_digest(&root.path().join("missing")).is_err());
    }
}
