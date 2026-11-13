//! Refusal of a node whose operator key material is still where an older
//! layout kept it.
//!
//! The operator key and genesis live under `.ai/node/operator/`. A node that
//! still holds them elsewhere must be moved by hand: nothing reads the old
//! files, and initialization must not mint a second operator identity beside
//! them. This is a refusal, never a reader.

use std::path::{Path, PathBuf};

use ryeos_engine::roots::RuntimeRoot;

/// The operator key or genesis is still at an old location.
#[derive(Debug)]
pub struct OperatorKeyOldLayout {
    /// `(old path, new path)` for each file found at an old location.
    pub moves: Vec<(PathBuf, PathBuf)>,
}

impl OperatorKeyOldLayout {
    pub fn code(&self) -> &'static str {
        "operator_key_old_layout"
    }

    /// The exact shell commands that move the files.
    pub fn cure(&self) -> String {
        let mut lines = Vec::new();
        for (old, new) in &self.moves {
            if let Some(parent) = new.parent() {
                lines.push(format!("mkdir -p '{}'", parent.display()));
            }
            lines.push(format!("mv '{}' '{}'", old.display(), new.display()));
        }
        lines.join(" && ")
    }
}

impl std::fmt::Display for OperatorKeyOldLayout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let found = self
            .moves
            .iter()
            .map(|(old, _)| old.display().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        write!(
            f,
            "operator_key_old_layout: operator key material is still at an old location ({found}). \
             Nothing reads it there and `ryeos init` will not create a second operator key beside \
             it. With the node stopped, move it: {}",
            self.cure()
        )
    }
}

impl std::error::Error for OperatorKeyOldLayout {}

fn old_paths(app_root: &Path) -> [(PathBuf, PathBuf); 2] {
    let runtime = RuntimeRoot::new(app_root.to_path_buf());
    let config = runtime.config();
    [
        (
            config.join("keys").join("signing").join("private_key.pem"),
            runtime.operator_signing_key_path(),
        ),
        (
            config.join("identity").join("operator-genesis.json"),
            runtime.operator_genesis_path(),
        ),
    ]
}

/// Report the old-layout operator files present under `app_root`.
pub fn check_operator_layout(app_root: &Path) -> Result<(), OperatorKeyOldLayout> {
    let moves: Vec<_> = old_paths(app_root)
        .into_iter()
        .filter(|(old, _)| std::fs::symlink_metadata(old).is_ok())
        .collect();
    if moves.is_empty() {
        Ok(())
    } else {
        Err(OperatorKeyOldLayout { moves })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_key_and_genesis_are_refused_with_the_exact_move() {
        let root = tempfile::tempdir().unwrap();
        check_operator_layout(root.path()).expect("a fresh root is fine");
        let (old_key, new_key) = old_paths(root.path())[0].clone();
        std::fs::create_dir_all(old_key.parent().unwrap()).unwrap();
        std::fs::write(&old_key, "k").unwrap();
        let error = check_operator_layout(root.path()).unwrap_err();
        assert_eq!(error.code(), "operator_key_old_layout");
        let text = error.to_string();
        assert!(text.contains(&old_key.display().to_string()), "{text}");
        assert!(text.contains(&format!("'{}' '{}'", old_key.display(), new_key.display())));
        assert_eq!(error.moves.len(), 1);

        let (old_genesis, _) = old_paths(root.path())[1].clone();
        std::fs::create_dir_all(old_genesis.parent().unwrap()).unwrap();
        std::fs::write(&old_genesis, "{}").unwrap();
        assert_eq!(
            check_operator_layout(root.path()).unwrap_err().moves.len(),
            2
        );
    }
}
