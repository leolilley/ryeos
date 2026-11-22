//! Data-only schema and borrowed root view for the admitted GPU loader closure.
//!
//! The borrowed view contains only references to an existing realization set
//! and descriptor authorities. It cannot export descriptors, authorize a
//! launch, or replace the App's independent comparison with retained inputs.

pub mod startup_v3;

use std::collections::BTreeSet;

use anyhow::ensure;
use serde::{Deserialize, Serialize};

pub const GPU_LOADER_CLOSURE_INVENTORY_SCHEMA: &str = "ryeos.gpu_loader_closure_inventory.v1";

const ROOT_ROLES: [GpuLoaderRootRole; 7] = [
    GpuLoaderRootRole::PythonGnu,
    GpuLoaderRootRole::Driver,
    GpuLoaderRootRole::Runtime,
    GpuLoaderRootRole::Tinygrad,
    GpuLoaderRootRole::Toolchain,
    GpuLoaderRootRole::Model,
    GpuLoaderRootRole::WorkerSource,
];

const EXTERNAL_REALIZATION_ROLES: [GpuLoaderRootRole; 6] = [
    GpuLoaderRootRole::PythonGnu,
    GpuLoaderRootRole::Driver,
    GpuLoaderRootRole::Runtime,
    GpuLoaderRootRole::Tinygrad,
    GpuLoaderRootRole::Toolchain,
    GpuLoaderRootRole::Model,
];

fn external_role_positions(ids: &[&str]) -> anyhow::Result<[usize; 6]> {
    ensure!(
        ids.len() == EXTERNAL_REALIZATION_ROLES.len(),
        "GPU loader borrowed roots require the complete six-realization set"
    );
    let mut positions = [usize::MAX; 6];
    let mut seen = BTreeSet::new();
    for (index, id) in ids.iter().enumerate() {
        let role = GpuLoaderRootRole::from_realization_id(id)?;
        let role_index = EXTERNAL_REALIZATION_ROLES
            .iter()
            .position(|candidate| *candidate == role)
            .ok_or_else(|| anyhow::anyhow!("GPU loader external root role is not selected"))?;
        ensure!(
            seen.insert(role),
            "GPU loader external root role is duplicated"
        );
        positions[role_index] = index;
    }
    ensure!(
        positions.iter().all(|index| *index != usize::MAX),
        "GPU loader borrowed roots omit a selected external role"
    );
    Ok(positions)
}

/// Fixed product/source aliases. They are schema roles, never input paths.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GpuLoaderRootRole {
    PythonGnu,
    Driver,
    Runtime,
    Tinygrad,
    Toolchain,
    Model,
    WorkerSource,
}

impl GpuLoaderRootRole {
    pub const fn alias(self) -> &'static str {
        match self {
            Self::PythonGnu => "python-gnu",
            Self::Driver => "driver",
            Self::Runtime => "runtime",
            Self::Tinygrad => "tinygrad",
            Self::Toolchain => "toolchain",
            Self::Model => "model",
            Self::WorkerSource => "worker-source",
        }
    }

    fn from_realization_id(id: &str) -> anyhow::Result<Self> {
        match id {
            "python-gnu" => Ok(Self::PythonGnu),
            "driver" => Ok(Self::Driver),
            "runtime" => Ok(Self::Runtime),
            "tinygrad" => Ok(Self::Tinygrad),
            "toolchain" => Ok(Self::Toolchain),
            "model" => Ok(Self::Model),
            _ => anyhow::bail!("unselected GPU loader realization role `{id}`"),
        }
    }
}

/// One exact selected realization paired with its already-retained descriptor.
pub struct BorrowedGpuLoaderRealizationRoot<'a> {
    realization: &'a ryeos_state::objects::ExternalContentRealization,
    source: &'a lillux::InheritedDescriptorAuthority,
}

impl<'a> BorrowedGpuLoaderRealizationRoot<'a> {
    pub fn realization(&self) -> &'a ryeos_state::objects::ExternalContentRealization {
        self.realization
    }

    pub fn source(&self) -> &'a lillux::InheritedDescriptorAuthority {
        self.source
    }

    pub fn role(&self) -> anyhow::Result<GpuLoaderRootRole> {
        GpuLoaderRootRole::from_realization_id(&self.realization.id)
    }
}

/// Data-only borrowed view. It neither clones descriptor authority nor proves
/// agreement with App's original selection; App must perform that join.
pub struct BorrowedGpuLoaderRealizationRoots<'a> {
    roots: Vec<BorrowedGpuLoaderRealizationRoot<'a>>,
}

impl<'a> BorrowedGpuLoaderRealizationRoots<'a> {
    /// Construct a view over one actual retained set and its positionally
    /// associated descriptors. Callers cannot turn this into launch authority.
    pub fn from_retained_parts(
        realizations: &'a ryeos_state::objects::ExternalContentRealizationSet,
        sources: &'a [lillux::InheritedDescriptorAuthority],
    ) -> anyhow::Result<Self> {
        use ryeos_state::objects::{ExternalContentKind, ExternalContentMode};

        realizations.validate()?;
        ensure!(
            realizations.iter().len() == EXTERNAL_REALIZATION_ROLES.len()
                && sources.len() == EXTERNAL_REALIZATION_ROLES.len(),
            "GPU loader borrowed roots require the complete six-realization set"
        );
        let entries = realizations.iter().collect::<Vec<_>>();
        let ids = entries
            .iter()
            .map(|entry| entry.id.as_str())
            .collect::<Vec<_>>();
        let positions = external_role_positions(&ids)?;
        let mut roots = Vec::with_capacity(EXTERNAL_REALIZATION_ROLES.len());
        let mut descriptor_identities = Vec::with_capacity(sources.len());
        for index in positions {
            let realization = entries[index];
            let source = &sources[index];
            ensure!(
                realization.kind == ExternalContentKind::Tree
                    && realization.mode == ExternalContentMode::Pinned,
                "GPU loader borrowed realization is not a pinned tree"
            );
            let identity = source.directory_identity()?;
            ensure!(
                !descriptor_identities.contains(&identity),
                "GPU loader root descriptors alias one materialized directory"
            );
            descriptor_identities.push(identity);
            roots.push(BorrowedGpuLoaderRealizationRoot {
                realization,
                source,
            });
        }
        Ok(Self { roots })
    }

    pub fn as_slice(&self) -> &[BorrowedGpuLoaderRealizationRoot<'a>] {
        &self.roots
    }

    pub fn len(&self) -> usize {
        self.roots.len()
    }

    pub fn is_empty(&self) -> bool {
        self.roots.is_empty()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GpuLoaderInventoryEntryKind {
    Directory,
    RegularFile,
    Symlink,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GpuLoaderInventoryRoot {
    /// Number of inventory entries for this root, including implied source dirs.
    pub entry_count: u64,
    pub manifest_sha256: String,
    pub mount_alias: String,
    pub role: GpuLoaderRootRole,
    pub selected_id: String,
    /// Sum of regular-file byte lengths for this root.
    pub total_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GpuLoaderInventoryEntry {
    pub kind: GpuLoaderInventoryEntryKind,
    pub mode: Option<u32>,
    pub relative_path: String,
    pub role: GpuLoaderRootRole,
    pub sha256: Option<String>,
    pub size_bytes: u64,
    /// Present only on source files, preserving the exact logical source root
    /// while `relative_path` remains the staged filesystem coordinate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_root: Option<String>,
    pub symlink_target: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GpuLoaderInventoryTotals {
    pub implied_source_directories: u64,
    pub manifest_rows: u64,
    pub staged_bytes: u64,
    pub staged_inodes: u64,
}

/// Canonical sealed inventory schema. It has no descriptor, host path, launch,
/// deadline, funding, device, or cancellation field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GpuLoaderClosureInventoryV1 {
    pub entries: Vec<GpuLoaderInventoryEntry>,
    pub roots: Vec<GpuLoaderInventoryRoot>,
    pub schema: String,
    pub selection_sha256: String,
    pub totals: GpuLoaderInventoryTotals,
}

impl GpuLoaderClosureInventoryV1 {
    pub fn validate(
        &self,
        maximum_entries: u64,
        maximum_inodes: u64,
        maximum_bytes: u64,
    ) -> anyhow::Result<()> {
        ensure!(
            self.schema == GPU_LOADER_CLOSURE_INVENTORY_SCHEMA,
            "GPU loader inventory schema is unsupported"
        );
        require_hash(&self.selection_sha256, "GPU loader selection digest")?;
        ensure!(
            self.roots.len() == ROOT_ROLES.len(),
            "GPU loader inventory root count differs from fixed roles"
        );
        for (root, role) in self.roots.iter().zip(ROOT_ROLES) {
            ensure!(
                root.role == role && root.mount_alias == role.alias(),
                "GPU loader inventory root role or alias is not canonical"
            );
            let expected_selected_id = if role == GpuLoaderRootRole::WorkerSource {
                "source"
            } else {
                role.alias()
            };
            ensure!(
                root.selected_id == expected_selected_id,
                "GPU loader selected root id differs from its fixed role"
            );
            require_hash(&root.manifest_sha256, "GPU loader root manifest hash")?;
        }

        ensure!(
            self.entries.len() as u64 <= maximum_entries,
            "GPU loader inventory exceeds the selected row bound"
        );
        ensure!(
            self.totals.staged_inodes <= maximum_inodes
                && self.totals.staged_bytes <= maximum_bytes,
            "GPU loader inventory totals exceed selected source limits"
        );
        let mut prior: Option<(GpuLoaderRootRole, Option<&str>, &str)> = None;
        let mut seen = BTreeSet::new();
        let mut bytes = 0u64;
        let mut counts = [0u64; 7];
        let mut bytes_by_root = [0u64; 7];
        for entry in &self.entries {
            validate_relative_path(&entry.relative_path)?;
            if entry.role == GpuLoaderRootRole::WorkerSource
                && entry.kind == GpuLoaderInventoryEntryKind::RegularFile
            {
                ensure!(
                    entry.source_root.as_deref() == Some("source"),
                    "GPU loader source-file row differs from the fixed source root"
                );
            } else {
                ensure!(
                    entry.source_root.is_none(),
                    "GPU loader non-source-file row carries a source root"
                );
            }
            let key = (
                entry.role,
                entry.source_root.as_deref(),
                entry.relative_path.as_str(),
            );
            ensure!(
                prior.is_none_or(|previous| previous < key),
                "GPU loader inventory entries are not strictly ordered"
            );
            prior = Some(key);
            ensure!(
                seen.insert((
                    entry.role,
                    entry.source_root.as_deref(),
                    entry.relative_path.as_str()
                )),
                "GPU loader inventory duplicates a member"
            );
            let role_index = ROOT_ROLES
                .iter()
                .position(|role| *role == entry.role)
                .ok_or_else(|| anyhow::anyhow!("GPU loader inventory has an unknown root role"))?;
            counts[role_index] = counts[role_index]
                .checked_add(1)
                .ok_or_else(|| anyhow::anyhow!("GPU loader row count overflow"))?;
            match entry.kind {
                GpuLoaderInventoryEntryKind::Directory => {
                    ensure!(
                        entry.size_bytes == 0
                            && entry.sha256.is_none()
                            && entry.symlink_target.is_none(),
                        "GPU loader directory row has file or symlink data"
                    );
                }
                GpuLoaderInventoryEntryKind::RegularFile => {
                    ensure!(
                        entry.symlink_target.is_none(),
                        "GPU loader regular-file row has a symlink target"
                    );
                    require_hash(
                        entry.sha256.as_deref().unwrap_or_default(),
                        "GPU loader file digest",
                    )?;
                    bytes_by_root[role_index] = bytes_by_root[role_index]
                        .checked_add(entry.size_bytes)
                        .ok_or_else(|| anyhow::anyhow!("GPU loader root byte total overflow"))?;
                    bytes = bytes
                        .checked_add(entry.size_bytes)
                        .ok_or_else(|| anyhow::anyhow!("GPU loader byte total overflow"))?;
                }
                GpuLoaderInventoryEntryKind::Symlink => {
                    ensure!(
                        entry.size_bytes == 0 && entry.sha256.is_none(),
                        "GPU loader symlink row is malformed"
                    );
                    let target = entry
                        .symlink_target
                        .as_deref()
                        .ok_or_else(|| anyhow::anyhow!("GPU loader symlink target is missing"))?;
                    ensure!(!target.is_empty(), "GPU loader symlink target is empty");
                    ryeos_state::objects::validate_internal_symlink_target(
                        &entry.relative_path,
                        target.as_bytes(),
                    )?;
                }
            }
        }
        let mut summed_rows = 0u64;
        let mut summed_bytes = 0u64;
        for (index, root) in self.roots.iter().enumerate() {
            ensure!(
                counts[index] == root.entry_count,
                "GPU loader root entry count differs from its rows"
            );
            ensure!(
                bytes_by_root[index] == root.total_bytes,
                "GPU loader root byte total differs from its regular-file rows"
            );
            summed_rows = summed_rows
                .checked_add(root.entry_count)
                .ok_or_else(|| anyhow::anyhow!("GPU loader root row total overflow"))?;
            summed_bytes = summed_bytes
                .checked_add(root.total_bytes)
                .ok_or_else(|| anyhow::anyhow!("GPU loader root byte total overflow"))?;
        }
        let expected_rows = self
            .totals
            .manifest_rows
            .checked_add(self.totals.implied_source_directories)
            .ok_or_else(|| anyhow::anyhow!("GPU loader inventory row total overflow"))?;
        ensure!(
            summed_rows == self.entries.len() as u64
                && summed_bytes == bytes
                && self.totals.staged_bytes == bytes
                && expected_rows == summed_rows,
            "GPU loader inventory aggregate totals disagree with rows"
        );
        let expected_inodes = 2u64
            .checked_add(self.roots.len() as u64)
            .and_then(|value| value.checked_add(summed_rows))
            .ok_or_else(|| anyhow::anyhow!("GPU loader inode total overflow"))?;
        ensure!(
            self.totals.staged_inodes == expected_inodes,
            "GPU loader inventory inode total differs from staged tree"
        );
        Ok(())
    }
}

fn require_hash(value: &str, label: &str) -> anyhow::Result<()> {
    ensure!(
        value.len() == 64
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        "{label} is not a SHA-256 hex digest"
    );
    Ok(())
}

fn validate_relative_path(value: &str) -> anyhow::Result<()> {
    ensure!(
        !value.is_empty() && value.len() <= 4096 && !value.starts_with('/'),
        "GPU loader inventory path is empty, absolute, or too long"
    );
    ryeos_state::objects::validate_canonical_project_relative_path(value)?;
    for component in value.split('/') {
        ensure!(
            !component.is_empty() && component != "." && component != "..",
            "GPU loader inventory path is not canonical"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn digest() -> String {
        "a".repeat(64)
    }

    fn empty_inventory() -> GpuLoaderClosureInventoryV1 {
        GpuLoaderClosureInventoryV1 {
            schema: GPU_LOADER_CLOSURE_INVENTORY_SCHEMA.to_owned(),
            selection_sha256: digest(),
            roots: ROOT_ROLES
                .into_iter()
                .map(|role| GpuLoaderInventoryRoot {
                    role,
                    selected_id: if role == GpuLoaderRootRole::WorkerSource {
                        "source".to_owned()
                    } else {
                        role.alias().to_owned()
                    },
                    mount_alias: role.alias().to_owned(),
                    manifest_sha256: digest(),
                    entry_count: 0,
                    total_bytes: 0,
                })
                .collect(),
            entries: Vec::new(),
            totals: GpuLoaderInventoryTotals {
                manifest_rows: 0,
                implied_source_directories: 0,
                staged_inodes: 9,
                staged_bytes: 0,
            },
        }
    }

    #[test]
    fn inventory_rejects_noncanonical_roles_and_overflowing_row_totals() {
        let mut wrong_role = empty_inventory();
        wrong_role.roots.swap(0, 1);
        assert!(wrong_role.validate(64, 128, 1024).is_err());

        let mut overflowing = empty_inventory();
        overflowing.totals.manifest_rows = u64::MAX;
        overflowing.totals.implied_source_directories = 1;
        assert!(overflowing.validate(u64::MAX, u64::MAX, u64::MAX).is_err());

        let valid = empty_inventory();
        assert!(valid.validate(64, 128, 1024).is_ok());
    }

    #[test]
    fn inventory_rejects_per_root_byte_shifts_with_unchanged_global_total() {
        let mut inventory = empty_inventory();
        inventory.roots[0].entry_count = 1;
        inventory.roots[0].total_bytes = 4;
        inventory.roots[1].entry_count = 1;
        inventory.roots[1].total_bytes = 6;
        inventory.entries = vec![
            GpuLoaderInventoryEntry {
                kind: GpuLoaderInventoryEntryKind::RegularFile,
                mode: Some(0o644),
                relative_path: "a.bin".to_owned(),
                role: GpuLoaderRootRole::PythonGnu,
                sha256: Some(digest()),
                size_bytes: 4,
                source_root: None,
                symlink_target: None,
            },
            GpuLoaderInventoryEntry {
                kind: GpuLoaderInventoryEntryKind::RegularFile,
                mode: Some(0o644),
                relative_path: "b.bin".to_owned(),
                role: GpuLoaderRootRole::Driver,
                sha256: Some(digest()),
                size_bytes: 6,
                source_root: None,
                symlink_target: None,
            },
        ];
        inventory.totals.manifest_rows = 2;
        inventory.totals.staged_inodes = 11;
        inventory.totals.staged_bytes = 10;
        assert!(inventory.validate(64, 128, 1024).is_ok());

        // Shift one byte between roots while preserving the aggregate sum.
        inventory.roots[0].total_bytes = 5;
        inventory.roots[1].total_bytes = 5;
        assert!(inventory.validate(64, 128, 1024).is_err());
    }

    #[test]
    fn inventory_rejects_selected_ids_that_do_not_match_fixed_roles() {
        let mut wrong_external_id = empty_inventory();
        wrong_external_id.roots[0].selected_id = "python".to_owned();
        assert!(wrong_external_id.validate(64, 128, 1024).is_err());

        let mut wrong_source_id = empty_inventory();
        wrong_source_id.roots[6].selected_id = "worker-source".to_owned();
        assert!(wrong_source_id.validate(64, 128, 1024).is_err());
    }

    #[test]
    fn inventory_rejects_worker_source_rows_from_another_logical_root() {
        let mut inventory = empty_inventory();
        inventory.roots[6].entry_count = 1;
        inventory.roots[6].total_bytes = 1;
        inventory.entries.push(GpuLoaderInventoryEntry {
            kind: GpuLoaderInventoryEntryKind::RegularFile,
            mode: Some(0o644),
            relative_path: "entry.py".to_owned(),
            role: GpuLoaderRootRole::WorkerSource,
            sha256: Some(digest()),
            size_bytes: 1,
            source_root: Some("source".to_owned()),
            symlink_target: None,
        });
        inventory.totals.manifest_rows = 1;
        inventory.totals.staged_inodes = 10;
        inventory.totals.staged_bytes = 1;
        assert!(inventory.validate(64, 128, 1024).is_ok());

        inventory.entries[0].source_root = Some("other".to_owned());
        assert!(inventory.validate(64, 128, 1024).is_err());
    }

    #[test]
    fn borrowed_root_roles_are_joined_by_identity_after_canonical_set_sorting() {
        // ExternalContentRealizationSet sorts identifiers, so role mapping
        // must not assume the fixed mount-role order matches wire order.
        let positions = external_role_positions(&[
            "driver",
            "model",
            "python-gnu",
            "runtime",
            "tinygrad",
            "toolchain",
        ])
        .unwrap();
        assert_eq!(positions, [2, 0, 3, 4, 5, 1]);
        assert!(
            external_role_positions(&[
                "driver",
                "driver",
                "python-gnu",
                "runtime",
                "tinygrad",
                "toolchain",
            ])
            .is_err()
        );
        assert!(
            external_role_positions(&[
                "driver",
                "model",
                "python-gnu",
                "runtime",
                "tinygrad",
                "unknown",
            ])
            .is_err()
        );
    }
}
