//! Named selection from an already retained lossless output manifest.
//!
//! This is bounded immutable metadata selection, not terminal or publication
//! authority. The caller proves the capture's ownership and successful result;
//! the existing witness publication owner verifies selected payload bytes.

use anyhow::{Context as _, bail};

use super::products::{ProductDeclaration, ProductShape, ProductSource, ProductStorage};
use super::{ExternalCapturePolicy, LargeContentCaptureBounds};
use crate::ignore::{IgnoreConfig, IgnoreMatcher};
use crate::object_closure::{ObjectClosureLimits, load_exact_cas_object_with_cas};
use crate::objects::{
    ExternalContentManifestEntryKind as EntryKind, ExternalContentManifestObject,
    ExternalLargeContentManifestObject, ProjectSnapshotPolicy, WorkspaceOutputCapture,
    WorkspaceOutputCaptureState,
};

/// Existing manifest formats only; selecting a subtree never copies or
/// converts payloads, flattens links, or changes a storage tier.
pub enum RetainedWorkspaceOutputContent {
    Content(ExternalContentManifestObject),
    LargeContent(ExternalLargeContentManifestObject),
}

impl RetainedWorkspaceOutputContent {
    #[allow(clippy::too_many_arguments)]
    pub fn select(
        cas: &lillux::CasStore,
        capture: &WorkspaceOutputCapture,
        declaration: &ProductDeclaration,
        exact_snapshot_policy: &ProjectSnapshotPolicy,
        current_node_ignore: &IgnoreMatcher,
        current_node_bounds: &LargeContentCaptureBounds,
        object_limits: ObjectClosureLimits,
    ) -> anyhow::Result<Option<Self>> {
        capture.validate()?;
        declaration.validate()?;
        exact_snapshot_policy.validate()?;
        current_node_bounds.validate()?;
        let partition = &capture.partition;
        let snapshot = crate::project_materialization::load_project_snapshot_bounded(
            cas,
            &capture.result_project_snapshot_hash,
        )?
        .context("workspace-output capture result snapshot is unavailable")?;
        partition.validate_result_source_policy(&snapshot, exact_snapshot_policy)?;
        if !partition
            .products
            .iter()
            .any(|product| product == declaration)
        {
            bail!("workspace-output product is not the exact admitted partition declaration");
        }
        let ProductSource::WorkspaceOutput { root } = &declaration.source else {
            bail!("retained-project selection cannot use a workspace-output capture");
        };
        let root = partition
            .roots
            .iter()
            .find(|candidate| &candidate.name == root)
            .context("workspace-output product root is not retained")?;
        if root.storage != declaration.storage {
            bail!("workspace-output product changes its root's storage tier");
        }
        let relative = if declaration.path == root.path {
            ""
        } else {
            declaration
                .path
                .strip_prefix(&format!("{}/", root.path))
                .context("workspace-output product escapes its declared root")?
        };
        let retained_ignore = IgnoreMatcher::from_config(&IgnoreConfig {
            patterns: exact_snapshot_policy.node_patterns.clone(),
        })?;
        // Both policies must permit the complete locator before optional
        // absence is considered. Current policy may refuse, never re-filter.
        for matcher in [&retained_ignore, current_node_ignore] {
            ExternalCapturePolicy::new(declaration.path.clone(), matcher)?;
        }
        let bounds = declaration.bounds.intersect(current_node_bounds)?;
        match &capture.outputs[&root.name] {
            WorkspaceOutputCaptureState::Absent => return absent(declaration),
            WorkspaceOutputCaptureState::EmptyDirectory if !relative.is_empty() => {
                return absent(declaration);
            }
            WorkspaceOutputCaptureState::EmptyDirectory => {
                bail!("present empty workspace output cannot be published as a product");
            }
            WorkspaceOutputCaptureState::Captured { manifest_hash, .. } => {
                let max_bytes = match root.storage {
                    ProductStorage::Content => crate::objects::MAX_EXTERNAL_CONTENT_MANIFEST_BYTES,
                    ProductStorage::LargeContent => {
                        crate::objects::MAX_LARGE_CONTENT_MANIFEST_BYTES
                    }
                } as u64;
                let value = load_exact_cas_object_with_cas(
                    cas,
                    manifest_hash,
                    max_bytes.min(object_limits.max_object_bytes),
                )?;
                let selected = match root.storage {
                    ProductStorage::Content => {
                        let manifest = ExternalContentManifestObject::from_value(&value)?;
                        let members = manifest
                            .entries
                            .iter()
                            .map(|entry| (entry.path.as_str(), entry.kind, entry.size))
                            .collect::<Vec<_>>();
                        let Some(selection) = select_members(
                            &members,
                            relative,
                            declaration,
                            &root.path,
                            &root.effective_bounds,
                            manifest.total_bytes,
                            &bounds,
                            &retained_ignore,
                            current_node_ignore,
                        )?
                        else {
                            return absent(declaration);
                        };
                        let entries = selection
                            .into_iter()
                            .map(|(index, path)| {
                                let mut entry = manifest.entries[index].clone();
                                entry.path = path;
                                entry
                            })
                            .collect::<Vec<_>>();
                        let selected = ExternalContentManifestObject {
                            schema: manifest.schema,
                            kind: manifest.kind,
                            entry_count: entries.len(),
                            total_bytes: entries.iter().filter_map(|entry| entry.size).sum(),
                            entries,
                        };
                        // In particular, links safe in the full root must
                        // remain contained after selecting/rebasing a subtree.
                        selected.validate()?;
                        Self::Content(selected)
                    }
                    ProductStorage::LargeContent => {
                        let manifest = ExternalLargeContentManifestObject::from_value(&value)?;
                        let members = manifest
                            .entries
                            .iter()
                            .map(|entry| (entry.path.as_str(), entry.kind, entry.size))
                            .collect::<Vec<_>>();
                        let Some(selection) = select_members(
                            &members,
                            relative,
                            declaration,
                            &root.path,
                            &root.effective_bounds,
                            manifest.total_bytes,
                            &bounds,
                            &retained_ignore,
                            current_node_ignore,
                        )?
                        else {
                            return absent(declaration);
                        };
                        let entries = selection
                            .into_iter()
                            .map(|(index, path)| {
                                let mut entry = manifest.entries[index].clone();
                                entry.path = path;
                                entry
                            })
                            .collect::<Vec<_>>();
                        let selected = ExternalLargeContentManifestObject {
                            schema: manifest.schema,
                            kind: manifest.kind,
                            entry_count: entries.len(),
                            total_bytes: entries.iter().filter_map(|entry| entry.size).sum(),
                            entries,
                        };
                        selected.validate()?;
                        Self::LargeContent(selected)
                    }
                };
                if let Some(expected) = &declaration.expected_manifest_hash {
                    if &selected.manifest_hash()? != expected {
                        bail!("workspace-output product differs from its expected manifest");
                    }
                }
                Ok(Some(selected))
            }
        }
    }

    pub fn to_value(&self) -> anyhow::Result<serde_json::Value> {
        match self {
            Self::Content(manifest) => {
                manifest.validate()?;
                Ok(serde_json::to_value(manifest)?)
            }
            Self::LargeContent(manifest) => manifest.to_value(),
        }
    }

    pub fn manifest_hash(&self) -> anyhow::Result<String> {
        crate::objects::canonical_value_digest(&self.to_value()?)
    }

    pub fn manifest_kind(&self) -> &str {
        match self {
            Self::Content(manifest) => &manifest.kind,
            Self::LargeContent(manifest) => &manifest.kind,
        }
    }

    pub fn entry_count(&self) -> usize {
        match self {
            Self::Content(manifest) => manifest.entry_count,
            Self::LargeContent(manifest) => manifest.entry_count,
        }
    }

    pub fn total_bytes(&self) -> u64 {
        match self {
            Self::Content(manifest) => manifest.total_bytes,
            Self::LargeContent(manifest) => manifest.total_bytes,
        }
    }
}

fn absent(
    declaration: &ProductDeclaration,
) -> anyhow::Result<Option<RetainedWorkspaceOutputContent>> {
    if declaration.required {
        bail!(
            "required workspace-output product is absent: {}",
            declaration.name
        );
    }
    Ok(None)
}

#[allow(clippy::too_many_arguments)]
fn select_members(
    members: &[(&str, EntryKind, Option<u64>)],
    relative: &str,
    declaration: &ProductDeclaration,
    root_path: &str,
    root_bounds: &super::products::ProductBounds,
    root_bytes: u64,
    selected_bounds: &LargeContentCaptureBounds,
    retained_ignore: &IgnoreMatcher,
    current_ignore: &IgnoreMatcher,
) -> anyhow::Result<Option<Vec<(usize, String)>>> {
    if members.is_empty()
        || members.len() > root_bounds.maximum_entries
        || root_bytes > root_bounds.maximum_total_bytes
    {
        bail!("captured workspace root contradicts its admitted bounds");
    }
    for &(path, kind, size) in members {
        ensure_entry_bounds(
            path,
            kind,
            size,
            root_bounds.maximum_depth,
            root_bounds.maximum_file_bytes,
        )?;
    }
    let find = |path: &str| members.binary_search_by_key(&path, |member| member.0).ok();
    // A missing member under a retained file/link is malformed selection,
    // not optional absence. Never resolve a symlink as a product locator.
    for (end, _) in relative.match_indices('/') {
        match find(&relative[..end]) {
            None => return Ok(None),
            Some(index) if members[index].1 == EntryKind::Dir => {}
            Some(_) => bail!("workspace-output product has a non-directory ancestor"),
        }
    }
    let selected_root = if relative.is_empty() {
        None
    } else {
        let Some(index) = find(relative) else {
            return Ok(None);
        };
        Some(index)
    };
    match declaration.shape {
        ProductShape::File
            if selected_root.is_none_or(|index| members[index].1 != EntryKind::File) =>
        {
            bail!("file product must select a retained regular file, never a directory or symlink");
        }
        ProductShape::Tree
            if selected_root.is_some_and(|index| members[index].1 != EntryKind::Dir) =>
        {
            bail!("tree product must select a retained directory, never a file or symlink");
        }
        _ => {}
    }
    let prefix = format!("{relative}/");
    let mut selected = Vec::new();
    let mut total = 0u64;
    for (index, &(path, kind, size)) in members.iter().enumerate() {
        let output_path = match declaration.shape {
            ProductShape::File if Some(index) == selected_root => {
                crate::objects::FILE_REALIZATION_ENTRY_PATH
            }
            ProductShape::File => continue,
            ProductShape::Tree if relative.is_empty() => path,
            ProductShape::Tree => match path.strip_prefix(&prefix) {
                Some(path) => path,
                None => continue,
            },
        };
        for matcher in [retained_ignore, current_ignore] {
            ExternalCapturePolicy::new(format!("{root_path}/{path}"), matcher)?;
        }
        ensure_entry_bounds(
            output_path,
            kind,
            size,
            selected_bounds.max_depth,
            selected_bounds.max_file_bytes,
        )?;
        total = total
            .checked_add(size.unwrap_or(0))
            .context("selected output byte count overflow")?;
        if selected.len() >= selected_bounds.max_entries || total > selected_bounds.max_total_bytes
        {
            bail!("workspace-output product exceeds its admitted selection bounds");
        }
        selected.push((index, output_path.to_owned()));
    }
    if selected.is_empty() {
        bail!("present empty workspace-output product has no publishable manifest entries");
    }
    Ok(Some(selected))
}

fn ensure_entry_bounds(
    path: &str,
    kind: EntryKind,
    size: Option<u64>,
    max_depth: usize,
    max_file_bytes: u64,
) -> anyhow::Result<()> {
    let depth = path
        .split('/')
        .count()
        .saturating_sub(usize::from(kind != EntryKind::Dir));
    if depth >= max_depth
        || (kind == EntryKind::File && size.is_none_or(|size| size > max_file_bytes))
    {
        bail!("retained output entry contradicts its admitted bounds: {path}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::external_content::products::ProductBounds;
    use crate::objects::workspace_output_capture::*;
    use serde_json::json;

    struct Fixture {
        _temporary: tempfile::TempDir,
        cas: lillux::CasStore,
        capture: WorkspaceOutputCapture,
        declaration: ProductDeclaration,
        policy: ProjectSnapshotPolicy,
    }

    fn bounds() -> ProductBounds {
        ProductBounds {
            maximum_entries: 32,
            maximum_depth: 8,
            maximum_file_bytes: 1024,
            maximum_total_bytes: 4096,
        }
    }

    fn ignore(patterns: &[&str]) -> IgnoreMatcher {
        IgnoreMatcher::from_config(&IgnoreConfig {
            patterns: patterns
                .iter()
                .map(|pattern| (*pattern).to_owned())
                .collect(),
        })
        .unwrap()
    }

    impl Fixture {
        fn new(storage: ProductStorage) -> Self {
            let temporary = tempfile::tempdir().unwrap();
            let cas = lillux::CasStore::new(temporary.path().join("objects"));
            let policy = ProjectSnapshotPolicy::from_matcher(
                crate::project_sync::ProjectSyncScope::FullProject,
                &ignore(&[]),
            )
            .unwrap();
            let policy_hash = cas.store_object(&policy.to_value()).unwrap();
            let snapshot = crate::objects::ProjectSnapshot {
                project_tree_hash: cas
                    .store_object(
                        &crate::objects::ProjectTree {
                            files: Default::default(),
                        }
                        .to_value(),
                    )
                    .unwrap(),
                effective_policy_hash: policy_hash.clone(),
                message: None,
                parent_hashes: Vec::new(),
                created_at: "2026-09-08T00:00:00Z".to_owned(),
                source: "output-selection-test".to_owned(),
            };
            let snapshot_hash = cas.store_object(&snapshot.to_value()).unwrap();
            let blob = cas.store_blob(b"program").unwrap();
            let entries = json!([
                {"path":"dist","kind":"dir"},
                {"path":"dist/bin","kind":"dir"},
                {"path":"dist/bin/program","kind":"file","mode":493,"blob_hash":blob,"size":7},
                {"path":"dist/bin/run","kind":"symlink","target":"program"},
                {"path":"dist/empty","kind":"dir"},
                {"path":"outside","kind":"file","mode":420,"blob_hash":blob,"size":7}
            ]);
            let kind = match storage {
                ProductStorage::Content => crate::objects::EXTERNAL_CONTENT_MANIFEST_KIND,
                ProductStorage::LargeContent => {
                    crate::objects::EXTERNAL_LARGE_CONTENT_MANIFEST_KIND
                }
            };
            let schema = match storage {
                ProductStorage::Content => crate::objects::EXTERNAL_CONTENT_TREE_SCHEMA,
                ProductStorage::LargeContent => crate::objects::EXTERNAL_LARGE_CONTENT_SCHEMA,
            };
            let manifest_hash = cas.store_object(&json!({"schema":schema,"kind":kind,"entries":entries,"entry_count":6,"total_bytes":14})).unwrap();
            let declaration = ProductDeclaration {
                name: "runtime".to_owned(),
                source: ProductSource::WorkspaceOutput {
                    root: "distribution".to_owned(),
                },
                path: "products/dist".to_owned(),
                shape: ProductShape::Tree,
                storage,
                required: true,
                bounds: bounds(),
                expected_manifest_hash: None,
            };
            let mut partition = WorkspaceOutputPartition {
                schema: WORKSPACE_OUTPUT_PARTITION_SCHEMA.to_owned(),
                recipe_binding: "product_recipe".to_owned(),
                recipe_ref: "config:test/products".to_owned(),
                recipe_raw_content_digest: "a".repeat(64),
                declarations_hash: "b".repeat(64),
                project_snapshot_policy_hash: policy_hash,
                roots: vec![WorkspaceOutputRoot {
                    name: "distribution".to_owned(),
                    path: "products".to_owned(),
                    storage,
                    declared_bounds: bounds(),
                    effective_bounds: bounds(),
                }],
                products: vec![declaration.clone()],
                partition_identity: "0".repeat(64),
                capture_policy_digest: "0".repeat(64),
            };
            partition.partition_identity = partition.derived_partition_identity().unwrap();
            partition.capture_policy_digest =
                partition.derived_capture_policy_digest(&policy).unwrap();
            let capture = WorkspaceOutputCapture {
                schema: WORKSPACE_OUTPUT_CAPTURE_SCHEMA.to_owned(),
                kind: WORKSPACE_OUTPUT_CAPTURE_KIND.to_owned(),
                producer_chain_root_id: "T-source".to_owned(),
                producer_thread_id: "T-terminal".to_owned(),
                admitted_launch_capsule_hash: "c".repeat(64),
                base_project_snapshot_hash: snapshot_hash.clone(),
                result_project_snapshot_hash: snapshot_hash,
                partition,
                outputs: std::collections::BTreeMap::from([(
                    "distribution".to_owned(),
                    WorkspaceOutputCaptureState::Captured {
                        manifest_kind: kind.to_owned(),
                        manifest_hash,
                    },
                )]),
            };
            capture.validate().unwrap();
            Self {
                _temporary: temporary,
                cas,
                capture,
                declaration,
                policy,
            }
        }

        fn update_declaration(&mut self, modify: impl FnOnce(&mut ProductDeclaration)) {
            modify(&mut self.declaration);
            self.capture.partition.products = vec![self.declaration.clone()];
            self.capture.partition.partition_identity =
                self.capture.partition.derived_partition_identity().unwrap();
            self.capture.validate().unwrap();
        }

        fn select(
            &self,
            current_ignore: &IgnoreMatcher,
            limits: &LargeContentCaptureBounds,
        ) -> anyhow::Result<Option<RetainedWorkspaceOutputContent>> {
            RetainedWorkspaceOutputContent::select(
                &self.cas,
                &self.capture,
                &self.declaration,
                &self.policy,
                current_ignore,
                limits,
                ObjectClosureLimits::default(),
            )
        }

        fn node_bounds() -> LargeContentCaptureBounds {
            LargeContentCaptureBounds {
                max_entries: 32,
                max_depth: 8,
                max_file_bytes: 1024,
                max_total_bytes: 4096,
            }
        }
    }

    #[test]
    fn both_tiers_select_exact_tree_and_file_preserving_links_modes_and_empty_directories() {
        for storage in [ProductStorage::Content, ProductStorage::LargeContent] {
            let mut fixture = Fixture::new(storage);
            let selected = fixture
                .select(&ignore(&[]), &Fixture::node_bounds())
                .unwrap()
                .unwrap();
            let value = selected.to_value().unwrap();
            assert_eq!(selected.entry_count(), 4);
            assert_eq!(selected.total_bytes(), 7);
            assert_eq!(value["entries"][1]["mode"], 493);
            assert_eq!(value["entries"][2]["target"], "program");
            assert_eq!(value["entries"][3]["path"], "empty");
            let expected = selected.manifest_hash().unwrap();
            fixture.update_declaration(|declaration| {
                declaration.expected_manifest_hash = Some(expected.clone())
            });
            assert_eq!(
                fixture
                    .select(&ignore(&[]), &Fixture::node_bounds())
                    .unwrap()
                    .unwrap()
                    .manifest_hash()
                    .unwrap(),
                expected
            );
            fixture.update_declaration(|declaration| {
                declaration.expected_manifest_hash = Some("d".repeat(64))
            });
            assert!(
                fixture
                    .select(&ignore(&[]), &Fixture::node_bounds())
                    .is_err()
            );
            fixture.update_declaration(|declaration| {
                declaration.expected_manifest_hash = None;
                declaration.path = "products/dist/bin/program".to_owned();
                declaration.shape = ProductShape::File;
            });
            let selected = fixture
                .select(&ignore(&[]), &Fixture::node_bounds())
                .unwrap()
                .unwrap();
            assert_eq!(
                selected.to_value().unwrap()["entries"][0]["path"],
                crate::objects::FILE_REALIZATION_ENTRY_PATH
            );
            fixture.update_declaration(|declaration| {
                declaration.path = "products/dist/bin/run".to_owned()
            });
            assert!(
                fixture
                    .select(&ignore(&[]), &Fixture::node_bounds())
                    .is_err()
            );
        }
    }

    #[test]
    fn retained_selection_does_not_reopen_non_owning_historical_base() {
        let mut fixture = Fixture::new(ProductStorage::Content);
        fixture.capture.base_project_snapshot_hash = "f".repeat(64);
        fixture.capture.validate().unwrap();

        assert!(
            fixture
                .select(&ignore(&[]), &Fixture::node_bounds())
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn both_tiers_reject_links_escaping_a_selected_subtree() {
        for storage in [ProductStorage::Content, ProductStorage::LargeContent] {
            let mut fixture = Fixture::new(storage);
            let WorkspaceOutputCaptureState::Captured { manifest_hash, .. } =
                fixture.capture.outputs.get_mut("distribution").unwrap()
            else {
                unreachable!()
            };
            let mut value = fixture.cas.get_object(manifest_hash).unwrap().unwrap();
            // The link is valid in the original root but invalid once `dist`
            // is the manifest root. No path flattening or host resolution.
            value["entries"][3]["target"] = json!("../../outside");
            *manifest_hash = fixture.cas.store_object(&value).unwrap();
            assert!(
                fixture
                    .select(&ignore(&[]), &Fixture::node_bounds())
                    .is_err()
            );
        }
    }

    #[test]
    fn optional_absence_is_not_empty_wrong_shape_or_policy_refusal() {
        for storage in [ProductStorage::Content, ProductStorage::LargeContent] {
            let mut fixture = Fixture::new(storage);
            fixture.update_declaration(|declaration| {
                declaration.path = "products/missing".to_owned();
                declaration.required = false;
            });
            assert!(
                fixture
                    .select(&ignore(&[]), &Fixture::node_bounds())
                    .unwrap()
                    .is_none()
            );
            assert!(
                fixture
                    .select(&ignore(&["products/"]), &Fixture::node_bounds())
                    .is_err()
            );
            fixture.update_declaration(|declaration| {
                declaration.path = "products/outside/missing".to_owned()
            });
            assert!(
                fixture
                    .select(&ignore(&[]), &Fixture::node_bounds())
                    .is_err()
            );
            fixture.update_declaration(|declaration| {
                declaration.path = "products/dist/empty".to_owned()
            });
            assert!(
                fixture
                    .select(&ignore(&[]), &Fixture::node_bounds())
                    .is_err()
            );
            fixture.capture.outputs.insert(
                "distribution".to_owned(),
                WorkspaceOutputCaptureState::EmptyDirectory,
            );
            fixture.update_declaration(|declaration| declaration.path = "products".to_owned());
            assert!(
                fixture
                    .select(&ignore(&[]), &Fixture::node_bounds())
                    .is_err()
            );
            fixture.capture.outputs.insert(
                "distribution".to_owned(),
                WorkspaceOutputCaptureState::Absent,
            );
            assert!(
                fixture
                    .select(&ignore(&[]), &Fixture::node_bounds())
                    .unwrap()
                    .is_none()
            );
            fixture.update_declaration(|declaration| declaration.required = true);
            assert!(
                fixture
                    .select(&ignore(&[]), &Fixture::node_bounds())
                    .is_err()
            );
        }
    }

    #[test]
    fn exact_declaration_policy_and_current_narrowing_are_mandatory() {
        let mut fixture = Fixture::new(ProductStorage::Content);
        let mut tighter = Fixture::node_bounds();
        tighter.max_file_bytes = 6;
        assert!(fixture.select(&ignore(&[]), &tighter).is_err());
        assert!(
            fixture
                .select(&ignore(&["run"]), &Fixture::node_bounds())
                .is_err()
        );
        // Unselected sibling bytes are not a hidden second selected product.
        assert!(
            fixture
                .select(&ignore(&["outside"]), &Fixture::node_bounds())
                .is_ok()
        );
        fixture.declaration.required = false;
        assert!(
            fixture
                .select(&ignore(&[]), &Fixture::node_bounds())
                .is_err()
        );
        fixture.declaration = fixture.capture.partition.products[0].clone();
        fixture.declaration.source = ProductSource::RetainedProject {};
        assert!(
            fixture
                .select(&ignore(&[]), &Fixture::node_bounds())
                .is_err()
        );
        fixture.declaration.source = ProductSource::WorkspaceOutput {
            root: "other".to_owned(),
        };
        assert!(
            fixture
                .select(&ignore(&[]), &Fixture::node_bounds())
                .is_err()
        );
        fixture.declaration = fixture.capture.partition.products[0].clone();
        fixture.policy = ProjectSnapshotPolicy::from_matcher(
            crate::project_sync::ProjectSyncScope::FullProject,
            &ignore(&["*.tmp"]),
        )
        .unwrap();
        assert!(
            fixture
                .select(&ignore(&[]), &Fixture::node_bounds())
                .is_err()
        );
    }
}
