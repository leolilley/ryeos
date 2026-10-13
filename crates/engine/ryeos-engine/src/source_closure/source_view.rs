//! Read-only source views and canonical item selection shared by source
//! admission and owner signing. Callers select the root authority; constructing
//! a view or request grants no signing, admission or publication authority.

use super::{AuthoritativeSourceContent, SourceRootRequest, SourceRootSelection};
use crate::project_content::ProjectContentEntry;
use std::path::{Path, PathBuf};

/// Resolve the root executable from the canonical item coordinate and the
/// signed kind's extension order. Host paths are deliberately absent: the
/// typed content root has already selected the authority, while the kind
/// schema defines the only filenames that may represent this canonical ref.
/// Select the canonical source unit for both admission and owner signing.
/// This only selects bytes from the supplied authoritative view; it grants no
/// execution, signing, or publication authority to its caller.
pub fn item_namespace_source_request(
    source: &dyn AuthoritativeSourceContent,
    expected_kind: &str,
    kind_schema: &crate::kind_registry::KindSchema,
    canonical_ref: &str,
    expected_source_digest: &str,
    max_file_bytes: u64,
) -> anyhow::Result<(SourceRootRequest, String)> {
    let selected = canonical_item_source_path(
        source,
        expected_kind,
        kind_schema,
        canonical_ref,
        expected_source_digest,
        max_file_bytes,
    )?;
    let (_, bare_id) = canonical_ref
        .split_once(':')
        .expect("canonical item lookup validated the ref");
    let kind_root = Path::new(crate::AI_DIR).join(&kind_schema.directory);
    let components = Path::new(bare_id).components().collect::<Vec<_>>();
    if components.len() == 1 {
        let entry = selected
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| anyhow::anyhow!("source item path is not UTF-8"))?
            .to_owned();
        return Ok((
            SourceRootRequest {
                id: "source".to_owned(),
                selection: SourceRootSelection::File { path: selected },
            },
            entry,
        ));
    }

    let prefix = kind_root.join(components[0].as_os_str());
    let entry = selected
        .strip_prefix(&prefix)
        .map_err(|_| anyhow::anyhow!("source item escaped its canonical namespace"))?
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("source item path is not UTF-8"))?
        .to_owned();
    Ok((
        SourceRootRequest {
            id: "source".to_owned(),
            selection: SourceRootSelection::Tree { prefix },
        },
        entry,
    ))
}

/// Select a source unit no broader than the signed kind's item-namespace
/// ceiling. `item_directory` is the closed narrow form: it captures only the
/// canonical item's containing directory while retaining the same exact root
/// item and owner-signed-files testimony.
pub fn executor_source_request(
    source: &dyn AuthoritativeSourceContent,
    location: crate::source_closure::ExecutorSourceLocation,
    expected_kind: &str,
    kind_schema: &crate::kind_registry::KindSchema,
    canonical_ref: &str,
    expected_source_digest: &str,
    max_file_bytes: u64,
) -> anyhow::Result<(SourceRootRequest, String)> {
    match location {
        crate::source_closure::ExecutorSourceLocation::ItemNamespace => {
            item_namespace_source_request(
                source,
                expected_kind,
                kind_schema,
                canonical_ref,
                expected_source_digest,
                max_file_bytes,
            )
        }
        crate::source_closure::ExecutorSourceLocation::ItemDirectory => {
            item_directory_binding_root(canonical_ref)?;
            let selected = canonical_item_source_path(
                source,
                expected_kind,
                kind_schema,
                canonical_ref,
                expected_source_digest,
                max_file_bytes,
            )?;
            let (prefix, entry) = item_directory_selection(&selected)?;
            Ok((
                SourceRootRequest {
                    id: "source".to_owned(),
                    selection: SourceRootSelection::Tree { prefix },
                },
                entry,
            ))
        }
    }
}

pub fn item_directory_binding_root(canonical_ref: &str) -> anyhow::Result<String> {
    let bare_id = canonical_ref
        .split_once(':')
        .map(|(_, bare_id)| bare_id)
        .ok_or_else(|| anyhow::anyhow!("source owner ref is not canonical"))?;
    ryeos_state::objects::validate_canonical_project_relative_path(bare_id)?;
    Path::new(bare_id)
        .parent()
        .and_then(Path::to_str)
        .filter(|root| !root.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| anyhow::anyhow!("item_directory source owner has no containing directory"))
}

fn item_directory_selection(selected: &Path) -> anyhow::Result<(PathBuf, String)> {
    let prefix = selected
        .parent()
        .ok_or_else(|| anyhow::anyhow!("source item has no containing directory"))?
        .to_path_buf();
    let entry = selected
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow::anyhow!("source item path is not UTF-8"))?
        .to_owned();
    Ok((prefix, entry))
}

pub fn canonical_item_source_path(
    source: &dyn AuthoritativeSourceContent,
    expected_kind: &str,
    kind_schema: &crate::kind_registry::KindSchema,
    canonical_ref: &str,
    expected_source_digest: &str,
    max_file_bytes: u64,
) -> anyhow::Result<PathBuf> {
    let (kind, bare_id) = canonical_ref
        .split_once(':')
        .ok_or_else(|| anyhow::anyhow!("source owner ref is not canonical"))?;
    if kind != expected_kind {
        anyhow::bail!("source owner ref contradicts its signed kind");
    }
    ryeos_state::objects::validate_canonical_project_relative_path(bare_id)?;
    let kind_root = Path::new(crate::AI_DIR).join(&kind_schema.directory);

    let mut selected = None;
    for format in &kind_schema.extensions {
        let relative = kind_root.join(format!("{bare_id}{}", format.ext));
        let Some(bytes) = source.read_file(&relative, max_file_bytes)? else {
            continue;
        };
        if lillux::sha256_hex(&bytes) != expected_source_digest {
            anyhow::bail!(
                "canonical source coordinate contradicts the resolved executable identity"
            );
        }
        selected = Some(relative);
        break;
    }
    let selected = selected.ok_or_else(|| {
        anyhow::anyhow!("canonical source coordinate is absent from its typed content root")
    })?;
    Ok(selected)
}

/// Read-only, path-pinned source view shared by admission and owner signing.
/// Constructing this view does not authorize signing or execution.
pub struct DirectorySourceContent<'a> {
    root: lillux::PinnedDirectory,
    identity: String,
    configured_ignore: &'a ryeos_state::ignore::IgnoreMatcher,
}

impl<'a> DirectorySourceContent<'a> {
    pub fn new(
        root: &Path,
        identity: String,
        configured_ignore: &'a ryeos_state::ignore::IgnoreMatcher,
    ) -> anyhow::Result<Self> {
        let root = lillux::PinnedDirectory::open(root)?
            .ok_or_else(|| anyhow::anyhow!("source content root is unavailable"))?;
        Ok(Self {
            root,
            identity,
            configured_ignore,
        })
    }

    fn included(&self, relative: &Path) -> anyhow::Result<bool> {
        let relative = relative
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("source content path is not UTF-8"))?;
        Ok(
            !ryeos_state::project_sync::is_durable_content_capture_floor_excluded(relative)
                && !self.configured_ignore.is_ignored(relative),
        )
    }
}

impl AuthoritativeSourceContent for DirectorySourceContent<'_> {
    fn authority_identity(&self) -> Result<String, crate::error::EngineError> {
        self.root.ensure_path_binding().map_err(engine_internal)?;
        Ok(self.identity.clone())
    }

    fn list_files(
        &self,
        prefix: &Path,
        max_entries: usize,
    ) -> Result<Vec<ProjectContentEntry>, crate::error::EngineError> {
        // Empty explicitly selects the whole owned root. Every other prefix
        // is canonical relative data, never a host path or normalized escape.
        if !prefix.as_os_str().is_empty() {
            let relative = prefix
                .to_str()
                .ok_or_else(|| engine_internal(anyhow::anyhow!("source prefix is not UTF-8")))?;
            ryeos_state::objects::validate_canonical_project_relative_path(relative)
                .map_err(engine_internal)?;
        }
        self.root.ensure_path_binding().map_err(engine_internal)?;
        let mut selected = self.root.try_clone().map_err(engine_internal)?;
        if prefix.components().take(65).count() > 64 {
            return Err(engine_internal(anyhow::anyhow!(
                "source prefix exceeds traversal depth"
            )));
        }
        for component in prefix.components() {
            let std::path::Component::Normal(name) = component else {
                return Err(engine_internal(anyhow::anyhow!(
                    "source prefix is not contained"
                )));
            };
            selected = selected
                .open_child_directory(name)
                .map_err(engine_internal)?
                .ok_or_else(|| engine_internal(anyhow::anyhow!("source content root is absent")))?;
        }
        selected.ensure_path_binding().map_err(engine_internal)?;
        let mut entries = Vec::new();
        selected
            .visit_regular_files_bounded(
                lillux::DirectoryTraversalBudget::new(max_entries.saturating_mul(2), 64),
                |relative, _is_directory| {
                    let full = prefix.join(relative);
                    self.included(&full).map(|included| !included)
                },
                |relative, file| {
                    if entries.len() >= max_entries {
                        anyhow::bail!("source content exceeds its signed file ceiling");
                    }
                    let full = prefix.join(relative);
                    if !self.included(&full)? {
                        return Ok(());
                    }
                    let metadata = file.metadata()?;
                    #[cfg(unix)]
                    let normalized_mode = {
                        use std::os::unix::fs::PermissionsExt as _;
                        if metadata.permissions().mode() & 0o111 == 0 {
                            0o644
                        } else {
                            0o755
                        }
                    };
                    #[cfg(not(unix))]
                    let normalized_mode = 0o644;
                    let bytes = lillux::read_open_regular_file_bounded(
                        file,
                        ryeos_state::objects::MAX_SOURCE_FILE_BYTES,
                    )?;
                    entries.push(ProjectContentEntry {
                        relative_path: relative.to_path_buf(),
                        content_hash: lillux::sha256_hex(&bytes),
                        size: bytes.len() as u64,
                        normalized_mode,
                    });
                    Ok(())
                },
            )
            .map_err(engine_internal)?;
        selected.ensure_path_binding().map_err(engine_internal)?;
        self.root.ensure_path_binding().map_err(engine_internal)?;
        entries.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
        Ok(entries)
    }

    fn read_file(
        &self,
        path: &Path,
        max_bytes: u64,
    ) -> Result<Option<Vec<u8>>, crate::error::EngineError> {
        if !self.included(path).map_err(engine_internal)? {
            return Ok(None);
        }
        let file = self
            .root
            .open_pinned_regular_descendant(path, false)
            .map_err(engine_internal)?;
        let bytes = file
            .map(|file| file.read_bounded(max_bytes))
            .transpose()
            .map_err(engine_internal)?;
        self.root.ensure_path_binding().map_err(engine_internal)?;
        Ok(bytes)
    }
}

fn engine_internal(error: anyhow::Error) -> crate::error::EngineError {
    crate::error::EngineError::Internal(error.to_string())
}

#[cfg(test)]
mod controls {
    use super::*;
    #[test]
    fn source_census_prefixes_are_canonical_and_descriptor_rooted() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("nested")).unwrap();
        std::fs::write(root.path().join("nested/inside.txt"), b"inside").unwrap();
        std::fs::write(root.path().join("nested/hidden.txt"), b"ignored").unwrap();
        std::fs::write(outside.path().join("outside.txt"), b"outside").unwrap();
        let ignore =
            ryeos_state::ignore::IgnoreMatcher::from_config(&ryeos_state::ignore::IgnoreConfig {
                patterns: vec!["nested/hidden.txt".into()],
            })
            .unwrap();
        let source = DirectorySourceContent::new(root.path(), "selected".into(), &ignore).unwrap();
        assert!(source.list_files(outside.path(), 32).is_err());
        for invalid in [
            "../outside",
            "nested/../outside",
            "./nested",
            "nested/./child",
            "nested//child",
            "nested/",
        ] {
            assert!(
                source.list_files(Path::new(invalid), 32).is_err(),
                "accepted {invalid}"
            );
        }
        let nested = source.list_files(Path::new("nested"), 32).unwrap();
        assert_eq!(nested.len(), 1);
        assert_eq!(nested[0].relative_path, Path::new("inside.txt"));
        assert_eq!(nested[0].content_hash, lillux::sha256_hex(b"inside"));
        let whole = source.list_files(Path::new(""), 32).unwrap();
        assert_eq!(whole.len(), 1);
        assert_eq!(whole[0].relative_path, Path::new("nested/inside.txt"));
    }

    #[cfg(unix)]
    #[test]
    fn source_census_refuses_symlink_prefix_components() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::create_dir(outside.path().join("child")).unwrap();
        std::fs::write(outside.path().join("child/outside.txt"), b"outside").unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("escape")).unwrap();
        let ignore =
            ryeos_state::ignore::IgnoreMatcher::from_config(&ryeos_state::ignore::IgnoreConfig {
                patterns: Vec::new(),
            })
            .unwrap();
        let source = DirectorySourceContent::new(root.path(), "selected".into(), &ignore).unwrap();
        assert!(source.list_files(Path::new("escape/child"), 32).is_err());
        assert_eq!(
            std::fs::read(outside.path().join("child/outside.txt")).unwrap(),
            b"outside"
        );
    }

    #[test]
    fn item_directory_selection_is_exactly_the_owner_directory() {
        let selected = Path::new(".ai/tools/ryeos/development/runtime/runtime.yaml");
        let (prefix, entry) = item_directory_selection(selected).unwrap();
        assert_eq!(prefix, Path::new(".ai/tools/ryeos/development/runtime"));
        assert_eq!(entry, "runtime.yaml");
        assert!(!Path::new(".ai/tools/ryeos/development/sibling/tool.py").starts_with(prefix));
        assert_eq!(
            item_directory_binding_root(
                "tool:ryeos/development/authoring-environment-production/runtime"
            )
            .unwrap(),
            "ryeos/development/authoring-environment-production"
        );
        assert!(item_directory_binding_root("tool:runtime").is_err());
    }

    #[test]
    fn absent_extension_candidate_is_not_a_read_failure() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join(".ai/tools/test")).unwrap();
        std::fs::write(root.path().join(".ai/tools/test/item.yaml"), b"descriptor").unwrap();
        let ignore =
            ryeos_state::ignore::IgnoreMatcher::from_config(&ryeos_state::ignore::IgnoreConfig {
                patterns: Vec::new(),
            })
            .unwrap();
        let source = DirectorySourceContent::new(root.path(), "test".into(), &ignore).unwrap();
        assert_eq!(
            source
                .read_file(Path::new(".ai/tools/test/item.py"), 32)
                .unwrap(),
            None
        );
        assert_eq!(
            source
                .read_file(Path::new(".ai/tools/test/item.yaml"), 32)
                .unwrap(),
            Some(b"descriptor".to_vec())
        );
        assert!(
            source
                .read_file(Path::new(".ai/tools/test/item.yaml"), 2)
                .is_err()
        );
        assert!(source.read_file(Path::new("../outside"), 32).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn unsafe_extension_candidate_is_not_treated_as_absent() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join(".ai/tools/test")).unwrap();
        std::os::unix::fs::symlink("missing", root.path().join(".ai/tools/test/item.py")).unwrap();
        let ignore =
            ryeos_state::ignore::IgnoreMatcher::from_config(&ryeos_state::ignore::IgnoreConfig {
                patterns: Vec::new(),
            })
            .unwrap();
        let source = DirectorySourceContent::new(root.path(), "test".into(), &ignore).unwrap();
        assert!(
            source
                .read_file(Path::new(".ai/tools/test/item.py"), 32)
                .is_err()
        );
    }
}
