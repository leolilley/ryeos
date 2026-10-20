//! Bounded data-only inventory retained with the original pooled inputs.
//!
//! Construction is reserved to the original unique precontact guard. The
//! sealed record is not a descriptor table, launch recipe, or owner capability.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::io::Write as _;
use std::path::Path;

use anyhow::{Context as _, ensure};

use ryeos_gpu_loader_owner_contract::{
    BorrowedGpuLoaderRealizationRoots, GpuLoaderClosureInventoryV1, GpuLoaderInventoryEntry,
    GpuLoaderInventoryEntryKind, GpuLoaderInventoryRoot, GpuLoaderInventoryTotals,
    GpuLoaderRootRole,
};

struct ExpectedEntry {
    kind: GpuLoaderInventoryEntryKind,
    mode: Option<u32>,
    size: u64,
    sha256: Option<String>,
    target: Option<String>,
}

struct ParsedManifest {
    rows: Vec<GpuLoaderInventoryEntry>,
    expected: BTreeMap<String, ExpectedEntry>,
    entry_count: u64,
    total_bytes: u64,
    referenced_blobs: Vec<(String, u64)>,
}

pub(crate) struct SealedGpuLoaderInventory {
    descriptor: lillux::InheritedDescriptorAuthority,
    canonical_bytes: Vec<u8>,
    sha256: String,
}

impl SealedGpuLoaderInventory {
    pub(crate) fn seal(
        inventory: &GpuLoaderClosureInventoryV1,
        maximum_entries: u64,
        maximum_inodes: u64,
        maximum_staged_bytes: u64,
        maximum_bytes: u64,
    ) -> anyhow::Result<Self> {
        inventory.validate(maximum_entries, maximum_inodes, maximum_staged_bytes)?;
        let canonical_bytes = canonical_serialize_bounded(inventory, maximum_bytes)?;
        let sha256 = lillux::sha256_hex(&canonical_bytes);
        let descriptor =
            lillux::sealed_memfd(c"ryeos-gpu-loader-closure-inventory", &canonical_bytes)
                .map_err(anyhow::Error::msg)?;
        descriptor.require_write_sealed_regular()?;
        let (readback, _) = descriptor.read_regular_file_stable_bounded(maximum_bytes)?;
        ensure!(
            readback == canonical_bytes && lillux::sha256_hex(&readback) == sha256,
            "GPU loader inventory seal/readback changed"
        );
        Ok(Self {
            descriptor,
            canonical_bytes,
            sha256,
        })
    }

    pub(crate) fn validate_same_seal(&self, maximum_bytes: u64) -> anyhow::Result<()> {
        self.descriptor.require_write_sealed_regular()?;
        let (readback, _) = self
            .descriptor
            .read_regular_file_stable_bounded(maximum_bytes)?;
        ensure!(
            readback == self.canonical_bytes && lillux::sha256_hex(&readback) == self.sha256,
            "retained GPU loader inventory no longer matches its original seal"
        );
        Ok(())
    }
}

struct BoundedWriter {
    bytes: Vec<u8>,
    maximum: usize,
}

impl std::io::Write for BoundedWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let next = self.bytes.len().checked_add(bytes.len()).ok_or_else(|| {
            std::io::Error::other("GPU loader inventory serialization size overflow")
        })?;
        if next > self.maximum {
            return Err(std::io::Error::other(
                "GPU loader inventory exceeds its actual node object byte bound",
            ));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn canonical_serialize_bounded(
    inventory: &GpuLoaderClosureInventoryV1,
    maximum_bytes: u64,
) -> anyhow::Result<Vec<u8>> {
    ensure!(maximum_bytes > 0, "GPU loader inventory byte bound is zero");
    let maximum = usize::try_from(maximum_bytes)
        .context("GPU loader inventory byte bound exceeds addressable memory")?;
    let mut writer = BoundedWriter {
        bytes: Vec::with_capacity(maximum),
        maximum,
    };
    writer.write_all(b"{\"entries\":[")?;
    for (index, entry) in inventory.entries.iter().enumerate() {
        if index != 0 {
            writer.write_all(b",")?;
        }
        write_canonical_small_value(&mut writer, entry)?;
    }
    writer.write_all(b"],\"roots\":[")?;
    for (index, root) in inventory.roots.iter().enumerate() {
        if index != 0 {
            writer.write_all(b",")?;
        }
        write_canonical_small_value(&mut writer, root)?;
    }
    writer.write_all(b"],\"schema\":")?;
    write_canonical_small_value(&mut writer, &inventory.schema)?;
    writer.write_all(b",\"selection_sha256\":")?;
    write_canonical_small_value(&mut writer, &inventory.selection_sha256)?;
    writer.write_all(b",\"totals\":")?;
    write_canonical_small_value(&mut writer, &inventory.totals)?;
    writer.write_all(b"}")?;
    ensure!(
        !writer.bytes.is_empty(),
        "GPU loader inventory serialization is empty"
    );
    Ok(writer.bytes)
}

fn write_canonical_small_value<T: serde::Serialize>(
    writer: &mut BoundedWriter,
    value: &T,
) -> anyhow::Result<()> {
    let value = serde_json::to_value(value)?;
    let canonical = lillux::canonical_json(&value).map_err(anyhow::Error::msg)?;
    writer.write_all(canonical.as_bytes())?;
    Ok(())
}

impl super::VerifiedTrustedPooledSessionInputs {
    /// Build from the exact original selected products/source and the same
    /// borrowed materializations while the unique unfunded precontact guard is
    /// checked out. A failed attempt is sticky in this same owner.
    pub(crate) fn retain_gpu_loader_closure_inventory(
        &mut self,
        state: &crate::state::AppState,
        roots: BorrowedGpuLoaderRealizationRoots<'_>,
    ) -> anyhow::Result<()> {
        if self.gpu_loader_inventory.is_some() || self.gpu_loader_inventory_refused {
            self.gpu_loader_inventory_refused = true;
            anyhow::bail!("GPU loader inventory cannot be replaced or retried");
        }
        let result = self.build_gpu_loader_inventory(state, &roots);
        match result {
            Ok(inventory) => {
                self.gpu_loader_inventory = Some(inventory);
                Ok(())
            }
            Err(error) => {
                self.gpu_loader_inventory_refused = true;
                Err(error)
            }
        }
    }

    pub(crate) fn validate_gpu_loader_inventory_for_restore(
        &mut self,
        state: &crate::state::AppState,
    ) -> anyhow::Result<()> {
        let validation = (|| {
            ensure!(
                !self.gpu_loader_inventory_refused,
                "GPU loader inventory has a sticky precontact refusal"
            );
            if self.startup_contract_selection.is_some() {
                self.selected_gpu_loader_realizations()?;
                let inventory = self
                    .gpu_loader_inventory
                    .as_ref()
                    .context("selected GPU loader input has no retained closure inventory")?;
                let limits = state.node_policy
                    .require::<ryeos_node_runtime::node_policy::sections::object_closure::NodeObjectClosurePolicy>()?
                    .closure_limits()?;
                inventory.validate_same_seal(
                    limits.max_object_bytes.min(limits.max_total_object_bytes),
                )?;
            } else {
                ensure!(
                    self.gpu_loader_inventory.is_none(),
                    "unselected inputs unexpectedly retain a GPU loader inventory"
                );
            }
            Ok(())
        })();
        if validation.is_err() {
            self.gpu_loader_inventory_refused = true;
        }
        validation
    }

    fn build_gpu_loader_inventory(
        &self,
        state: &crate::state::AppState,
        roots: &BorrowedGpuLoaderRealizationRoots<'_>,
    ) -> anyhow::Result<SealedGpuLoaderInventory> {
        use ryeos_state::objects::{ExternalContentKind, ExternalContentMode};

        self.require_same_state(state)?;
        self.require_original_isolation_expectation(state)?;
        let original_deadline = self
            .original_isolation
            .as_ref()
            .context("GPU loader closure lacks original deadline")?
            .original_deadline;
        let state_guard = self
            .state_authority
            .acquire_shared_guard_until(original_deadline)?;
        self.state_authority.ensure_guard(&state_guard)?;
        let selected = self.selected_gpu_loader_realizations()?;
        let node_policy = state
            .node_policy
            .require::<ryeos_node_runtime::node_policy::sections::object_closure::NodeObjectClosurePolicy>(
        )?;
        let limits = node_policy.closure_limits()?;
        ensure!(
            node_policy.max_roots >= 8,
            "selected node object closure root ceiling cannot cover seven selected roots and the sealed inventory"
        );
        ensure!(
            roots.len() == selected.iter().len(),
            "GPU loader borrowed roots are not the actual complete selected set"
        );
        let products = self
            .original_product_inputs
            .as_ref()
            .context("GPU loader closure lacks actual selected GNU/driver products")?;
        let startup = self
            .original_startup_contract
            .as_ref()
            .context("GPU loader closure lacks actual startup contract")?;
        let source = &startup.source;
        let decoded_contract = startup
            .contract
            .as_ref()
            .context("GPU loader closure lacks decoded retained startup contract")?;
        let selected_entry = &decoded_contract.common().entry;
        let retained_member = startup
            .member
            .as_ref()
            .context("GPU loader closure lacks retained startup entry member")?;
        ensure!(
            selected_entry.root == retained_member.file.root
                && selected_entry.path == retained_member.file.path
                && selected_entry.sha256 == retained_member.file.blob_hash
                && selected_entry.root == "source"
                && lillux::sha256_hex(&retained_member.bytes) == selected_entry.sha256,
            "worker-source selected entry differs from its retained exact member"
        );
        let source_manifest = source.manifest();
        source_manifest.validate()?;
        ensure!(
            source_manifest.roots.len() == 1
                && source_manifest.roots[0].id == "source"
                && source_manifest
                    .entries
                    .iter()
                    .all(|entry| entry.root == "source"),
            "admitted worker source no longer satisfies the exact single-root execution invariant"
        );
        ensure!(
            source_manifest.entries.len() <= limits.max_links_per_object,
            "worker source manifest exceeds selected node link limit"
        );
        source
            .binding()
            .validate_content_manifest(source_manifest)?;
        ensure!(
            selected
                .iter()
                .len()
                .checked_add(3)
                .is_some_and(|count| count <= limits.max_objects),
            "GPU loader manifests and inventory exceed selected node object count"
        );

        ensure!(
            limits.max_object_bytes > 0 && limits.max_total_object_bytes > 0,
            "GPU loader closure has no finite selected node object bounds"
        );
        let cas = self.state_authority.cas_store()?;
        let mut inventory_roots = Vec::with_capacity(7);
        let mut inventory_entries = Vec::new();
        let mut external_rows = 0u64;
        let mut external_bytes = 0u64;
        let mut referenced_blobs = BTreeMap::<String, u64>::new();
        let mut manifest_document_bytes = 0u64;
        let mut source_rows = 0u64;
        let mut implied_source_directories = BTreeSet::new();

        // Roles are already canonicalized by the contract projection. The
        // App still compares each borrowed member to its independent selected
        // realization before trusting the descriptor for traversal.
        for borrowed in roots.as_slice() {
            let role = borrowed.role()?;
            let entry = borrowed.realization();
            let original = selected
                .iter()
                .find(|selected| selected.id == entry.id)
                .context("borrowed GPU realization is absent from original selected set")?;
            ensure!(
                original == entry
                    && entry.kind == ExternalContentKind::Tree
                    && entry.mode == ExternalContentMode::Pinned,
                "borrowed GPU root differs from its original complete selected realization"
            );
            let value = ryeos_state::object_closure::load_exact_cas_object_with_cas(
                &cas,
                &entry.manifest_hash,
                limits
                    .max_object_bytes
                    .min(ryeos_state::objects::MAX_LARGE_CONTENT_MANIFEST_BYTES as u64),
            )?;
            let value_bytes = lillux::canonical_json(&value)
                .map_err(anyhow::Error::msg)?
                .len() as u64;
            manifest_document_bytes = manifest_document_bytes
                .checked_add(value_bytes)
                .context("GPU loader manifest document byte total overflow")?;
            let mut manifest = parse_external_manifest(&value)?;
            for row in &mut manifest.rows {
                row.role = role;
            }
            ensure!(
                manifest.entry_count
                    <= u64::try_from(limits.max_links_per_object).unwrap_or(u64::MAX),
                "GPU loader manifest exceeds selected node link limit"
            );
            ensure!(
                manifest.entry_count == entry.entry_count as u64
                    && manifest.total_bytes == entry.total_bytes,
                "GPU loader realization count/byte selection differs from its exact manifest"
            );
            if role == GpuLoaderRootRole::PythonGnu {
                let bounds = &products.gnu.selection.relationship.required_product.bounds;
                validate_product_bounds(
                    &manifest.expected,
                    bounds.maximum_entries,
                    bounds.maximum_total_bytes,
                    bounds.maximum_file_bytes,
                    bounds.maximum_depth,
                )?;
                ensure!(
                    entry.manifest_hash == products.gnu.selection.manifest_hash
                        && manifest.entry_count == products.gnu.witness.evidence.entry_count as u64
                        && manifest.total_bytes == products.gnu.witness.evidence.total_bytes
                        && manifest.entry_count <= bounds.maximum_entries as u64
                        && manifest.total_bytes <= bounds.maximum_total_bytes,
                    "GNU realization differs from its retained product witness"
                );
            } else if role == GpuLoaderRootRole::Driver {
                let bounds = &products
                    .driver
                    .selection
                    .relationship
                    .required_product
                    .bounds;
                validate_product_bounds(
                    &manifest.expected,
                    bounds.maximum_entries,
                    bounds.maximum_total_bytes,
                    bounds.maximum_file_bytes,
                    bounds.maximum_depth,
                )?;
                ensure!(
                    entry.manifest_hash == products.driver.selection.manifest_hash
                        && manifest.entry_count
                            == products.driver.witness.evidence.entry_count as u64
                        && manifest.total_bytes == products.driver.witness.evidence.total_bytes
                        && manifest.entry_count <= bounds.maximum_entries as u64
                        && manifest.total_bytes <= bounds.maximum_total_bytes,
                    "driver realization differs from its retained product witness"
                );
            }
            verify_materialized_tree(borrowed.source(), &manifest.expected, manifest.entry_count)?;
            for (hash, bytes) in &manifest.referenced_blobs {
                if let Some(previous) = referenced_blobs.insert(hash.clone(), *bytes) {
                    ensure!(
                        previous == *bytes,
                        "GPU loader selected blob hash has conflicting selected sizes"
                    );
                }
            }
            external_rows = external_rows
                .checked_add(manifest.entry_count)
                .context("GPU loader external row count overflow")?;
            external_bytes = external_bytes
                .checked_add(manifest.total_bytes)
                .context("GPU loader external byte total overflow")?;
            inventory_roots.push(GpuLoaderInventoryRoot {
                role,
                selected_id: entry.id.clone(),
                mount_alias: role.alias().to_owned(),
                manifest_sha256: entry.manifest_hash.clone(),
                entry_count: manifest.entry_count,
                total_bytes: manifest.total_bytes,
            });
            inventory_entries.extend(manifest.rows);
        }

        // The worker-source tree is the same admitted source closure already
        // redeemed and verified by Executor before this precontact guard.
        // Its signed effective binding is independently rechecked here.
        let binding = source.binding();
        let source_hash = source_manifest.digest()?;
        ensure!(
            binding.content_manifest_hash == source_hash,
            "worker source manifest digest differs from selected effective binding"
        );
        let source_manifest_value = source_manifest.to_value()?;
        manifest_document_bytes = manifest_document_bytes
            .checked_add(
                lillux::canonical_json(&source_manifest_value)
                    .map_err(anyhow::Error::msg)?
                    .len() as u64,
            )
            .context("GPU loader source manifest document byte total overflow")?;
        let binding_value = binding.to_value()?;
        let binding_bytes = lillux::canonical_json(&binding_value)
            .map_err(anyhow::Error::msg)?
            .len() as u64;
        ensure!(
            binding_bytes <= limits.max_object_bytes,
            "GPU loader source binding exceeds selected per-object limit"
        );
        manifest_document_bytes = manifest_document_bytes
            .checked_add(binding_bytes)
            .context("GPU loader source binding document byte total overflow")?;
        ensure!(
            source_manifest
                .roots
                .iter()
                .any(|root| root.id == selected_entry.root),
            "worker-source selected entry root is absent from retained source manifest"
        );
        let source_selected_id = selected_entry.root.clone();
        let mut source_root_entries = Vec::new();
        for file in &source_manifest.entries {
            let relative = Path::new(&file.path);
            let mut parent = relative.parent();
            while let Some(path) = parent {
                if path.as_os_str().is_empty() {
                    break;
                }
                let key = path
                    .to_str()
                    .context("source directory path is not UTF-8")?
                    .to_owned();
                implied_source_directories.insert(key);
                parent = path.parent();
            }
            source_rows = source_rows
                .checked_add(1)
                .context("source row count overflow")?;
            source_root_entries.push(GpuLoaderInventoryEntry {
                role: GpuLoaderRootRole::WorkerSource,
                source_root: Some(file.root.clone()),
                relative_path: file.path.clone(),
                kind: GpuLoaderInventoryEntryKind::RegularFile,
                mode: Some(match file.mode {
                    ryeos_state::objects::SourceFileMode::ReadOnly => 0o644,
                    ryeos_state::objects::SourceFileMode::Executable => 0o755,
                }),
                size_bytes: file.size,
                sha256: Some(file.blob_hash.clone()),
                symlink_target: None,
            });
            if let Some(previous) = referenced_blobs.insert(file.blob_hash.clone(), file.size) {
                ensure!(
                    previous == file.size,
                    "GPU loader source blob hash has conflicting selected sizes"
                );
            }
        }
        for path in &implied_source_directories {
            source_root_entries.push(GpuLoaderInventoryEntry {
                role: GpuLoaderRootRole::WorkerSource,
                source_root: None,
                relative_path: path.clone(),
                kind: GpuLoaderInventoryEntryKind::Directory,
                mode: None,
                size_bytes: 0,
                sha256: None,
                symlink_target: None,
            });
        }
        let source_entry_count = source_rows
            .checked_add(implied_source_directories.len() as u64)
            .context("source directory row total overflow")?;
        let source_bytes = source_manifest.totals.total_bytes;
        inventory_roots.push(GpuLoaderInventoryRoot {
            role: GpuLoaderRootRole::WorkerSource,
            selected_id: source_selected_id,
            mount_alias: GpuLoaderRootRole::WorkerSource.alias().to_owned(),
            manifest_sha256: source_hash.clone(),
            entry_count: source_entry_count,
            total_bytes: source_bytes,
        });
        inventory_entries.extend(source_root_entries);
        inventory_entries.sort_by(|left, right| {
            (
                left.role,
                left.source_root.as_deref(),
                left.relative_path.as_bytes(),
            )
                .cmp(&(
                    right.role,
                    right.source_root.as_deref(),
                    right.relative_path.as_bytes(),
                ))
        });

        let source_directories = u64::try_from(implied_source_directories.len())
            .context("source directory count exceeds u64")?;
        let manifest_rows = external_rows
            .checked_add(source_rows)
            .context("GPU loader manifest row total overflow")?;
        let staged_bytes = external_bytes
            .checked_add(source_bytes)
            .context("GPU loader staged byte total overflow")?;
        let staged_inodes = 2u64
            .checked_add(inventory_roots.len() as u64)
            .and_then(|total| total.checked_add(manifest_rows))
            .and_then(|total| total.checked_add(source_directories))
            .context("GPU loader staged inode total overflow")?;
        let row_bound = (ryeos_state::objects::MAX_EXTERNAL_REALIZATION_ENTRIES as u64)
            .checked_mul(6)
            .and_then(|bound| bound.checked_add(ryeos_state::objects::MAX_SOURCE_FILES as u64))
            .and_then(|bound| bound.checked_add(source_directories))
            .context("GPU loader row bound overflow")?;
        let byte_bound = ryeos_state::objects::MAX_REALIZATION_CLAIMED_BYTES
            .checked_add(ryeos_state::objects::MAX_SOURCE_TOTAL_BYTES)
            .context("GPU loader byte bound overflow")?;
        let source_fs_limits = lillux::sandbox::LinuxPrivateSourceLimits {
            max_bytes: 64 * 1024 * 1024 * 1024,
            max_inodes: 2_000_000,
        };
        source_fs_limits.validate().map_err(anyhow::Error::msg)?;
        let total_blob_bytes = referenced_blobs.values().try_fold(0u64, |total, size| {
            total
                .checked_add(*size)
                .context("GPU loader selected blob byte total overflow")
        })?;
        ensure!(
            referenced_blobs.len() <= limits.max_blobs
                && total_blob_bytes <= limits.max_total_blob_bytes
                && referenced_blobs
                    .values()
                    .all(|size| *size <= limits.max_blob_bytes),
            "GPU loader referenced blobs exceed selected node closure limits"
        );
        ensure!(
            staged_bytes <= byte_bound
                && staged_bytes <= source_fs_limits.max_bytes
                && staged_inodes <= source_fs_limits.max_inodes,
            "GPU loader closure exceeds actual product/source or Lillux ceilings"
        );

        let selection_value = serde_json::json!({
            "realizations": selected,
            "worker_source_binding": binding.digest()?,
            "worker_source_manifest": source_hash,
        });
        let selection_canonical =
            lillux::canonical_json(&selection_value).map_err(anyhow::Error::msg)?;
        let inventory = GpuLoaderClosureInventoryV1 {
            schema: ryeos_gpu_loader_owner_contract::GPU_LOADER_CLOSURE_INVENTORY_SCHEMA.to_owned(),
            selection_sha256: lillux::sha256_hex(selection_canonical.as_bytes()),
            roots: inventory_roots,
            entries: inventory_entries,
            totals: GpuLoaderInventoryTotals {
                manifest_rows,
                implied_source_directories: source_directories,
                staged_inodes,
                staged_bytes,
            },
        };
        let inventory_object_limit = limits.max_object_bytes.min(
            limits
                .max_total_object_bytes
                .saturating_sub(manifest_document_bytes),
        );
        ensure!(
            inventory_object_limit > 0,
            "GPU loader inventory has no bounded node object capacity"
        );
        inventory.validate(row_bound, source_fs_limits.max_inodes, byte_bound)?;
        let sealed = SealedGpuLoaderInventory::seal(
            &inventory,
            row_bound,
            source_fs_limits.max_inodes,
            byte_bound,
            inventory_object_limit,
        )?;
        self.state_authority.ensure_guard(&state_guard)?;
        self.require_same_state(state)?;
        self.require_original_isolation_expectation(state)?;
        Ok(sealed)
    }
}

fn parse_external_manifest(value: &serde_json::Value) -> anyhow::Result<ParsedManifest> {
    if let Ok(manifest) =
        ryeos_state::objects::ExternalLargeContentManifestObject::from_value(value)
    {
        let mut rows = Vec::with_capacity(manifest.entries.len());
        let mut expected = BTreeMap::new();
        let mut referenced_blobs = Vec::new();
        for entry in &manifest.entries {
            let (kind, size, hash, target) = match entry.kind {
                ryeos_state::objects::ExternalContentManifestEntryKind::Dir => {
                    (GpuLoaderInventoryEntryKind::Directory, 0, None, None)
                }
                ryeos_state::objects::ExternalContentManifestEntryKind::File => (
                    GpuLoaderInventoryEntryKind::RegularFile,
                    entry.size.context("large manifest file lacks size")?,
                    Some(
                        entry
                            .file_sha256
                            .as_ref()
                            .or(entry.blob_hash.as_ref())
                            .context("large manifest file lacks exact digest")?
                            .clone(),
                    ),
                    None,
                ),
                ryeos_state::objects::ExternalContentManifestEntryKind::Symlink => (
                    GpuLoaderInventoryEntryKind::Symlink,
                    0,
                    None,
                    Some(
                        entry
                            .target
                            .clone()
                            .context("large manifest symlink lacks target")?,
                    ),
                ),
            };
            if entry.kind == ryeos_state::objects::ExternalContentManifestEntryKind::File {
                if let Some(chunk_size) = entry.chunk_size {
                    let size = entry.size.context("large chunked file lacks size")?;
                    let mut remaining = size;
                    for hash in &entry.chunk_hashes {
                        let chunk_bytes = remaining.min(chunk_size);
                        referenced_blobs.push((hash.clone(), chunk_bytes));
                        remaining = remaining
                            .checked_sub(chunk_bytes)
                            .context("large-content chunk size underflow")?;
                    }
                    ensure!(
                        remaining == 0,
                        "large-content chunks do not cover their selected file size"
                    );
                } else if let Some(hash) = &entry.blob_hash {
                    referenced_blobs.push((hash.clone(), entry.size.unwrap_or_default()));
                }
            }
            rows.push(GpuLoaderInventoryEntry {
                role: GpuLoaderRootRole::PythonGnu,
                source_root: None,
                relative_path: entry.path.clone(),
                kind,
                mode: entry.mode,
                size_bytes: size,
                sha256: hash.clone(),
                symlink_target: target.clone(),
            });
            expected.insert(
                entry.path.clone(),
                ExpectedEntry {
                    kind,
                    mode: entry.mode,
                    size,
                    sha256: hash,
                    target,
                },
            );
        }
        Ok(ParsedManifest {
            entry_count: manifest.entry_count as u64,
            total_bytes: manifest.total_bytes,
            rows,
            expected,
            referenced_blobs,
        })
    } else {
        let manifest = ryeos_state::objects::ExternalContentManifestObject::from_value(value)?;
        let mut rows = Vec::with_capacity(manifest.entries.len());
        let mut expected = BTreeMap::new();
        let mut referenced_blobs = Vec::new();
        for entry in &manifest.entries {
            let (kind, size, hash, target) = match entry.kind {
                ryeos_state::objects::ExternalContentManifestEntryKind::Dir => {
                    (GpuLoaderInventoryEntryKind::Directory, 0, None, None)
                }
                ryeos_state::objects::ExternalContentManifestEntryKind::File => (
                    GpuLoaderInventoryEntryKind::RegularFile,
                    entry.size.context("content manifest file lacks size")?,
                    Some(
                        entry
                            .blob_hash
                            .clone()
                            .context("content manifest file lacks digest")?,
                    ),
                    None,
                ),
                ryeos_state::objects::ExternalContentManifestEntryKind::Symlink => (
                    GpuLoaderInventoryEntryKind::Symlink,
                    0,
                    None,
                    Some(
                        entry
                            .target
                            .clone()
                            .context("content manifest symlink lacks target")?,
                    ),
                ),
            };
            if entry.kind == ryeos_state::objects::ExternalContentManifestEntryKind::File {
                referenced_blobs.push((
                    entry
                        .blob_hash
                        .clone()
                        .context("content manifest file lacks CAS blob hash")?,
                    entry.size.context("content manifest file lacks size")?,
                ));
            }
            rows.push(GpuLoaderInventoryEntry {
                role: GpuLoaderRootRole::PythonGnu,
                source_root: None,
                relative_path: entry.path.clone(),
                kind,
                mode: entry.mode,
                size_bytes: size,
                sha256: hash.clone(),
                symlink_target: target.clone(),
            });
            expected.insert(
                entry.path.clone(),
                ExpectedEntry {
                    kind,
                    mode: entry.mode,
                    size,
                    sha256: hash,
                    target,
                },
            );
        }
        Ok(ParsedManifest {
            entry_count: manifest.entry_count as u64,
            total_bytes: manifest.total_bytes,
            rows,
            expected,
            referenced_blobs,
        })
    }
}

fn validate_product_bounds(
    entries: &BTreeMap<String, ExpectedEntry>,
    maximum_entries: usize,
    maximum_total_bytes: u64,
    maximum_file_bytes: u64,
    maximum_depth: usize,
) -> anyhow::Result<()> {
    ensure!(
        entries.len() <= maximum_entries,
        "selected product entry bound is exceeded"
    );
    let mut total = 0u64;
    for (path, entry) in entries {
        if entry.kind == GpuLoaderInventoryEntryKind::RegularFile {
            ensure!(
                entry.size <= maximum_file_bytes,
                "selected product file bound is exceeded"
            );
            total = total
                .checked_add(entry.size)
                .context("selected product byte total overflow")?;
        }
        ensure!(
            Path::new(path).components().count() <= maximum_depth,
            "selected product path depth bound is exceeded"
        );
    }
    ensure!(
        total <= maximum_total_bytes,
        "selected product total byte bound is exceeded"
    );
    Ok(())
}

fn verify_materialized_tree(
    source: &lillux::InheritedDescriptorAuthority,
    expected: &BTreeMap<String, ExpectedEntry>,
    maximum_entries: u64,
) -> anyhow::Result<()> {
    let root = source.try_clone_pinned_directory(source.path().to_path_buf())?;
    let mut found = BTreeSet::new();
    walk_materialized_directory(&root, &root, "", expected, &mut found, maximum_entries, 0)?;
    ensure!(
        found.len() as u64 == maximum_entries && found.len() == expected.len(),
        "GPU loader materialized tree differs from exact manifest row count"
    );
    Ok(())
}

fn walk_materialized_directory(
    root: &lillux::PinnedDirectory,
    current: &lillux::PinnedDirectory,
    parent: &str,
    expected: &BTreeMap<String, ExpectedEntry>,
    found: &mut BTreeSet<String>,
    maximum_entries: u64,
    depth: usize,
) -> anyhow::Result<()> {
    ensure!(
        depth <= maximum_entries as usize,
        "GPU loader tree exceeds its row-derived traversal depth"
    );
    let remaining = maximum_entries.saturating_sub(found.len() as u64);
    let immediate_bound = usize::try_from(remaining).unwrap_or(usize::MAX);
    let children = current.entries_no_follow_bounded(immediate_bound)?;
    for child in children {
        let name = child
            .name
            .to_str()
            .context("GPU loader tree contains a non-UTF-8 path")?;
        let relative = if parent.is_empty() {
            name.to_owned()
        } else {
            format!("{parent}/{name}")
        };
        ensure!(
            found.insert(relative.clone()),
            "GPU loader tree contains a duplicate path"
        );
        let row = expected
            .get(&relative)
            .context("GPU loader tree contains an unmanifested path")?;
        ensure!(
            row.mode.is_none_or(|mode| mode == (child.mode & 0o7777)),
            "GPU loader materialized mode differs from manifest"
        );
        match child.entry_type {
            lillux::secure_fs::PinnedEntryType::Directory => {
                ensure!(
                    row.kind == GpuLoaderInventoryEntryKind::Directory,
                    "GPU loader directory differs from manifest kind"
                );
                let directory = current
                    .open_child_directory(OsStr::new(name))?
                    .context("GPU loader child directory disappeared during traversal")?;
                walk_materialized_directory(
                    root,
                    &directory,
                    &relative,
                    expected,
                    found,
                    maximum_entries,
                    depth + 1,
                )?;
            }
            lillux::secure_fs::PinnedEntryType::Regular => {
                ensure!(
                    row.kind == GpuLoaderInventoryEntryKind::RegularFile,
                    "GPU loader file differs from manifest kind"
                );
                let file = root
                    .open_pinned_regular_descendant(Path::new(&relative), false)?
                    .context("GPU loader file disappeared during traversal")?;
                let observation = file.observation()?;
                ensure!(
                    observation.size() == row.size,
                    "GPU loader materialized file size differs from manifest"
                );
                if let Some(mode) = row.mode {
                    ensure!(
                        file.permission_mode()? == mode,
                        "GPU loader materialized file mode differs from manifest"
                    );
                }
                let digest = file.digest_stable_exact(&observation)?;
                ensure!(
                    row.sha256.as_deref() == Some(digest.as_str()),
                    "GPU loader materialized file digest differs from manifest"
                );
            }
            lillux::secure_fs::PinnedEntryType::Symlink => {
                ensure!(
                    row.kind == GpuLoaderInventoryEntryKind::Symlink,
                    "GPU loader symlink differs from manifest kind"
                );
                let target = current
                    .read_symlink_target(OsStr::new(name), 4096)?
                    .context("GPU loader symlink disappeared during traversal")?;
                let target = std::str::from_utf8(&target)?;
                ensure!(
                    row.target.as_deref() == Some(target),
                    "GPU loader symlink target differs from manifest"
                );
            }
            _ => anyhow::bail!("GPU loader materialization contains a non-file tree object"),
        }
        ensure!(
            found.len() as u64 <= maximum_entries,
            "GPU loader materialization exceeds its manifest row bound"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_inventory_writer_matches_canonical_json_and_stops_at_cap() {
        let roles = [
            GpuLoaderRootRole::PythonGnu,
            GpuLoaderRootRole::Driver,
            GpuLoaderRootRole::Runtime,
            GpuLoaderRootRole::Tinygrad,
            GpuLoaderRootRole::Toolchain,
            GpuLoaderRootRole::Model,
            GpuLoaderRootRole::WorkerSource,
        ];
        let inventory = GpuLoaderClosureInventoryV1 {
            entries: vec![GpuLoaderInventoryEntry {
                role: GpuLoaderRootRole::WorkerSource,
                source_root: Some("source".to_owned()),
                relative_path: "α/𝄞".to_owned(),
                kind: GpuLoaderInventoryEntryKind::RegularFile,
                mode: Some(0o644),
                size_bytes: 1,
                sha256: Some("c".repeat(64)),
                symlink_target: None,
            }],
            roots: roles
                .into_iter()
                .map(|role| GpuLoaderInventoryRoot {
                    entry_count: u64::from(role == GpuLoaderRootRole::WorkerSource),
                    manifest_sha256: "a".repeat(64),
                    mount_alias: role.alias().to_owned(),
                    role,
                    selected_id: if role == GpuLoaderRootRole::WorkerSource {
                        "source".to_owned()
                    } else {
                        role.alias().to_owned()
                    },
                    total_bytes: u64::from(role == GpuLoaderRootRole::WorkerSource),
                })
                .collect(),
            schema: ryeos_gpu_loader_owner_contract::GPU_LOADER_CLOSURE_INVENTORY_SCHEMA.to_owned(),
            selection_sha256: "b".repeat(64),
            totals: GpuLoaderInventoryTotals {
                implied_source_directories: 0,
                manifest_rows: 1,
                staged_bytes: 1,
                staged_inodes: 10,
            },
        };
        assert!(inventory.validate(64, 128, 1024).is_ok());
        let value = serde_json::to_value(&inventory).unwrap();
        let canonical = lillux::canonical_json(&value).unwrap();
        assert_eq!(
            canonical_serialize_bounded(&inventory, 4096).unwrap(),
            canonical.as_bytes()
        );
        assert!(canonical_serialize_bounded(&inventory, 16).is_err());

        // This clone exists only to exercise canonical control-character
        // escaping. Its path is invalid, so it is not an accepted inventory.
        let mut encoding_only = inventory.clone();
        encoding_only.entries[0].relative_path = "α/𝄞\u{0001}".to_owned();
        assert!(encoding_only.validate(64, 128, 1024).is_err());
        let encoding_value = serde_json::to_value(&encoding_only).unwrap();
        let encoding_canonical = lillux::canonical_json(&encoding_value).unwrap();
        assert_eq!(
            canonical_serialize_bounded(&encoding_only, 4096).unwrap(),
            encoding_canonical.as_bytes()
        );
    }
}
