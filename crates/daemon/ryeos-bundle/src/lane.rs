//! Authority limits for a bundle in the publication lane.
//!
//! A bundle registered with a lane marker arrived through a publication pull:
//! its publisher signed the bytes and the node pinned that key, but nothing
//! built, qualified or attested it. Such a bundle may carry items, but it may
//! not carry authority: no kinds, no manifest-declared runtime authority, no
//! native executables and no node extensions. The check runs on the staged
//! tree at admission and again whenever the node loads its registrations, so a
//! tree edited after install is refused too.
//!
//! Commands need no check here. The node-config loader already refuses a
//! command item from any bundle that the node's `command_registration` policy
//! does not name (`UnnamedBundleCommand`), and a laned bundle is never named.

use std::fs;
use std::path::{Path, PathBuf};

use crate::manifest::BundleManifest;

/// Directories, relative to the bundle root, whose files extend the node
/// itself: handlers, parsers, runtimes and protocols. One entry per refused
/// directory, so ruling a new node-extension kind in is a one-line change.
const NODE_EXTENSION_DIRS: &[&str] = &[
    ".ai/handlers",
    ".ai/parsers",
    ".ai/runtimes",
    ".ai/protocols",
];

/// Directories, relative to the bundle root, that hold native executables.
const NATIVE_EXECUTABLE_DIRS: &[&str] = &["bin", ".ai/bin"];

/// The closing sentence of every refusal.
const LANE_LIMIT_NOT_LIFTABLE: &str =
    "This limit cannot be lifted on this build; there is no flag or policy that lifts it yet.";

/// Why a publication-lane bundle was refused. The message starts with the
/// stable code.
#[derive(Debug, thiserror::Error)]
pub enum LaneRefusal {
    #[error(
        "bundle_lane_kinds: the bundle manifest provides kinds {kinds:?}; a publication-lane bundle may not provide kinds. {}",
        LANE_LIMIT_NOT_LIFTABLE
    )]
    Kinds { kinds: Vec<String> },
    #[error(
        "bundle_lane_runtime_authority: the bundle manifest declares runtime authority ({families}); a publication-lane bundle may not declare any. {}",
        LANE_LIMIT_NOT_LIFTABLE
    )]
    RuntimeAuthority { families: String },
    #[error(
        "bundle_lane_native_executable: the bundle ships a native executable directory at {path}; a publication-lane bundle may not ship executables. {}",
        LANE_LIMIT_NOT_LIFTABLE
    )]
    NativeExecutable { path: String },
    #[error(
        "bundle_lane_node_extension: the bundle ships a node extension ({found}); a publication-lane bundle may not extend the node. {}",
        LANE_LIMIT_NOT_LIFTABLE
    )]
    NodeExtension { found: String },
    #[error(
        "bundle_lane_unreadable: the bundle tree at {path} could not be inspected ({detail}); a publication-lane bundle is refused when it cannot be checked. {}",
        LANE_LIMIT_NOT_LIFTABLE
    )]
    Unreadable { path: String, detail: String },
}

impl LaneRefusal {
    /// The stable code the message starts with.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Kinds { .. } => "bundle_lane_kinds",
            Self::RuntimeAuthority { .. } => "bundle_lane_runtime_authority",
            Self::NativeExecutable { .. } => "bundle_lane_native_executable",
            Self::NodeExtension { .. } => "bundle_lane_node_extension",
            Self::Unreadable { .. } => "bundle_lane_unreadable",
        }
    }
}

/// Refuse a publication-lane bundle that carries kinds, runtime authority,
/// native executables or node extensions.
pub fn refuse_lane_authority(
    bundle_root: &Path,
    manifest: &BundleManifest,
) -> Result<(), LaneRefusal> {
    if !manifest.provides_kinds.is_empty() {
        return Err(LaneRefusal::Kinds {
            kinds: manifest.provides_kinds.clone(),
        });
    }

    let families = runtime_authority_families(manifest);
    if !families.is_empty() {
        return Err(LaneRefusal::RuntimeAuthority {
            families: families.join(", "),
        });
    }

    if let Some(path) = native_executable_dir(bundle_root)? {
        return Err(LaneRefusal::NativeExecutable { path });
    }

    let mut found = node_extension_manifest_fields(manifest);
    for dir in NODE_EXTENSION_DIRS {
        if let Some(file) = first_non_directory(&bundle_root.join(dir))? {
            found.push(format!("file {}", display_relative(bundle_root, &file)));
        }
    }
    if !found.is_empty() {
        return Err(LaneRefusal::NodeExtension {
            found: found.join(", "),
        });
    }
    Ok(())
}

/// Names of the runtime-authority families the manifest declares.
fn runtime_authority_families(manifest: &BundleManifest) -> Vec<&'static str> {
    let authority = &manifest.runtime_authority;
    let mut families = Vec::new();
    if !authority.bundle_events.is_empty() {
        families.push("bundle_events");
    }
    if !authority.runtime_vault.is_empty() {
        families.push("runtime_vault");
    }
    if !authority.item_authoring.is_empty() {
        families.push("item_authoring");
    }
    if !authority.project_snapshots.is_empty() {
        families.push("project_snapshots");
    }
    if authority.state_home.is_some() {
        families.push("state_home");
    }
    if authority.operator_vault {
        families.push("operator_vault");
    }
    if !authority.route_capabilities.is_empty() {
        families.push("route_capabilities");
    }
    families
}

/// The native-executable check: a `bin` directory at the bundle root or
/// under `.ai` that holds any entry. Returns the relative path found.
fn native_executable_dir(bundle_root: &Path) -> Result<Option<String>, LaneRefusal> {
    for dir in NATIVE_EXECUTABLE_DIRS {
        let path = bundle_root.join(dir);
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(unreadable(&path, &error)),
        };
        if !metadata.is_dir() {
            return Ok(Some((*dir).to_string()));
        }
        let mut entries = fs::read_dir(&path).map_err(|error| unreadable(&path, &error))?;
        if entries.next().is_some() {
            return Ok(Some((*dir).to_string()));
        }
    }
    Ok(None)
}

/// Manifest declarations that extend the node: isolation backends, external
/// providers and external lifecycle adapters.
fn node_extension_manifest_fields(manifest: &BundleManifest) -> Vec<String> {
    let mut found = Vec::new();
    if !manifest.isolation_backends.is_empty() {
        found.push("manifest isolation_backends".to_string());
    }
    if !manifest.external_providers.is_empty() {
        found.push("manifest external_providers".to_string());
    }
    if !manifest.external_lifecycle_adapters.is_empty() {
        found.push("manifest external_lifecycle_adapters".to_string());
    }
    found
}

/// The first non-directory entry anywhere below `dir`, or none when `dir` is
/// absent or holds only empty directories.
fn first_non_directory(dir: &Path) -> Result<Option<PathBuf>, LaneRefusal> {
    let mut pending = vec![dir.to_path_buf()];
    while let Some(current) = pending.pop() {
        let entries = match fs::read_dir(&current) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(unreadable(&current, &error)),
        };
        for entry in entries {
            let entry = entry.map_err(|error| unreadable(&current, &error))?;
            let path = entry.path();
            let file_type = entry
                .file_type()
                .map_err(|error| unreadable(&path, &error))?;
            if file_type.is_dir() {
                pending.push(path);
            } else {
                return Ok(Some(path));
            }
        }
    }
    Ok(None)
}

fn display_relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .display()
        .to_string()
}

fn unreadable(path: &Path, error: &std::io::Error) -> LaneRefusal {
    LaneRefusal::Unreadable {
        path: path.display().to_string(),
        detail: error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{BundleEventDecl, BundleEventOperation, RuntimeAuthorityDecls};

    fn manifest() -> BundleManifest {
        BundleManifest {
            name: "pulled".to_string(),
            version: "1.0.0".to_string(),
            description: String::new(),
            provides_kinds: Vec::new(),
            requires_kinds: Vec::new(),
            uses_kinds: Vec::new(),
            runtime_authority: RuntimeAuthorityDecls::default(),
            smoke: Vec::new(),
            shadows: Vec::new(),
            owns_namespaces: Vec::new(),
            sync_include: Vec::new(),
            isolation_backends: Vec::new(),
            external_providers: Vec::new(),
            external_lifecycle_adapters: Vec::new(),
        }
    }

    fn write(root: &Path, rel: &str) {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, b"x").unwrap();
    }

    fn refusal(root: &Path, manifest: &BundleManifest) -> LaneRefusal {
        refuse_lane_authority(root, manifest).unwrap_err()
    }

    fn assert_message(error: &LaneRefusal, code: &str) {
        assert_eq!(error.code(), code);
        let message = error.to_string();
        assert!(message.starts_with(&format!("{code}:")), "{message}");
        assert!(message.ends_with(LANE_LIMIT_NOT_LIFTABLE), "{message}");
    }

    #[test]
    fn plain_bundle_passes() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), ".ai/knowledge/note.md");
        fs::create_dir_all(tmp.path().join("bin")).unwrap();
        fs::create_dir_all(tmp.path().join(".ai/handlers/empty")).unwrap();
        refuse_lane_authority(tmp.path(), &manifest()).unwrap();
    }

    #[test]
    fn provided_kinds_are_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let mut manifest = manifest();
        manifest.provides_kinds = vec!["widget".to_string()];
        assert_message(&refusal(tmp.path(), &manifest), "bundle_lane_kinds");
    }

    #[test]
    fn runtime_authority_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let mut manifest = manifest();
        manifest.runtime_authority.bundle_events = vec![BundleEventDecl {
            event_kind: "pulled.event".to_string(),
            operations: vec![BundleEventOperation::Append],
        }];
        let error = refusal(tmp.path(), &manifest);
        assert_message(&error, "bundle_lane_runtime_authority");
        assert!(error.to_string().contains("bundle_events"));

        let mut second = manifest();
        second.runtime_authority.operator_vault = true;
        second.runtime_authority.route_capabilities = vec!["ryeos.use.x".to_string()];
        let error = refusal(tmp.path(), &second);
        assert_message(&error, "bundle_lane_runtime_authority");
        assert!(
            error
                .to_string()
                .contains("operator_vault, route_capabilities")
        );
    }

    #[test]
    fn native_executable_directories_are_refused() {
        for rel in ["bin/tool", ".ai/bin/tool"] {
            let tmp = tempfile::tempdir().unwrap();
            write(tmp.path(), rel);
            assert_message(
                &refusal(tmp.path(), &manifest()),
                "bundle_lane_native_executable",
            );
        }
    }

    #[test]
    fn node_extension_directories_are_refused() {
        for dir in NODE_EXTENSION_DIRS {
            let tmp = tempfile::tempdir().unwrap();
            write(tmp.path(), &format!("{dir}/nested/item.yaml"));
            let error = refusal(tmp.path(), &manifest());
            assert_message(&error, "bundle_lane_node_extension");
            assert!(error.to_string().contains(dir), "{error}");
        }
    }

    #[test]
    fn unreadable_tree_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), ".ai/handlers");
        // `.ai/handlers` is a file, so reading it as a directory fails.
        assert_message(&refusal(tmp.path(), &manifest()), "bundle_lane_unreadable");
    }
}
