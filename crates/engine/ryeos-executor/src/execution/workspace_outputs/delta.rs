//! Fold the adapter's quiesced upper-layer mutations into retained output
//! manifests. The upper layer is never mistaken for a complete merged tree.

use anyhow::{Context, bail};
use ryeos_isolation_protocol::{WorkspaceMutation, WorkspaceMutationKind as Mutation};
use ryeos_state::external_content::products::ProductStorage;
use ryeos_state::objects::{
    ExternalContentManifestEntryKind as Kind, ExternalContentManifestObject,
    ExternalLargeContentManifestEntry as Entry, ExternalLargeContentManifestObject,
    ProjectSnapshotPolicy, WorkspaceOutputCapture, WorkspaceOutputCaptureState as State,
    WorkspaceOutputPartition, WorkspaceOutputRoot,
};
use std::collections::BTreeMap;

#[allow(clippy::too_many_arguments)]
pub(crate) fn apply_output_delta(
    authority: &ryeos_state::PinnedStateAuthority,
    guard: &ryeos_state::CasMutationGuard,
    stage: &mut ryeos_state::StagedCasRootLease,
    mutation_content: &lillux::PinnedDirectory,
    partition: &WorkspaceOutputPartition,
    policy: &ProjectSnapshotPolicy,
    base_capture: Option<&WorkspaceOutputCapture>,
    mutations: &[WorkspaceMutation],
) -> anyhow::Result<BTreeMap<String, State>> {
    authority.ensure_guard(guard)?;
    let matcher = super::validate_capture_policy(partition, policy)?;
    if mutations.len() > ryeos_isolation_protocol::MAX_WORKSPACE_MUTATIONS {
        bail!("workspace output mutation count exceeds the protocol bound");
    }
    for mutation in mutations {
        mutation.validate()?;
    }
    if mutations
        .windows(2)
        .any(|pair| pair[0].path >= pair[1].path)
    {
        bail!("workspace output mutations are not in unique canonical order");
    }
    if let Some(base) = base_capture {
        base.validate()?;
        if &base.partition != partition {
            bail!("output delta base does not retain the admitted partition");
        }
    }
    let cas = authority.cas_store()?;
    let mut outputs = BTreeMap::new();
    for root in &partition.roots {
        let capture_policy = ryeos_state::ExternalCapturePolicy::new(root.path.clone(), &matcher)?;
        let base = base_capture.map(|capture| &capture.outputs[&root.name]);
        let mut exists = base.is_some_and(|state| !matches!(state, State::Absent));
        let mut entries = load_entries(&cas, root, base, &matcher)?;
        let mut changed_files = BTreeMap::new();
        for mutation in mutations {
            let root_path = std::path::Path::new(&root.path);
            let path = std::path::Path::new(&mutation.path);
            if root_path.starts_with(path) {
                // Ancestor replacement hides the whole lower output. Only an
                // exact directory mutation establishes the reserved root itself.
                match mutation.kind {
                    Mutation::UpsertRegular | Mutation::UpsertSymlink => {
                        bail!(
                            "workspace output root or ancestor is no longer a directory: {}",
                            mutation.path
                        );
                    }
                    Mutation::DeletePath => {
                        exists = false;
                        entries.clear();
                    }
                    Mutation::OpaqueDirectory => {
                        exists = path == root_path;
                        entries.clear();
                    }
                    Mutation::EnsureDirectory if path == root_path => exists = true,
                    Mutation::EnsureDirectory => {}
                }
                continue;
            }
            let Ok(relative) = path.strip_prefix(root_path) else {
                continue;
            };
            let relative = relative
                .to_str()
                .context("non-UTF-8 output mutation path")?;
            if capture_policy.excludes_subtree_path(relative)? {
                continue;
            }
            if !exists {
                bail!(
                    "output mutation has no admitted directory root: {}",
                    mutation.path
                );
            }
            require_directory_ancestors(&entries, relative)?;
            match mutation.kind {
                Mutation::DeletePath => remove_subtree(&mut entries, relative),
                Mutation::EnsureDirectory => {
                    if entries
                        .get(relative)
                        .is_some_and(|entry| entry.kind != Kind::Dir)
                    {
                        remove_subtree(&mut entries, relative);
                    }
                    entries.insert(relative.to_owned(), empty_entry(relative, Kind::Dir));
                }
                Mutation::OpaqueDirectory => {
                    remove_subtree(&mut entries, relative);
                    entries.insert(relative.to_owned(), empty_entry(relative, Kind::Dir));
                }
                Mutation::UpsertSymlink => {
                    let target = mutation
                        .target
                        .as_ref()
                        .context("symlink mutation has no target")?;
                    ryeos_state::objects::validate_internal_symlink_target(
                        relative,
                        target.as_bytes(),
                    )?;
                    let (parent, name) = super::super::workspace::open_mutation_parent(
                        mutation_content,
                        &mutation.path,
                    )?;
                    let observed = parent.read_symlink_target(
                        &name,
                        ryeos_state::objects::MAX_SYMLINK_TARGET_BYTES as usize,
                    )?;
                    if observed.as_deref() != Some(target.as_bytes()) {
                        bail!(
                            "workspace output link differs from frozen evidence: {}",
                            mutation.path
                        );
                    }
                    parent.ensure_path_binding()?;
                    remove_subtree(&mut entries, relative);
                    let mut entry = empty_entry(relative, Kind::Symlink);
                    entry.target = Some(target.clone());
                    entries.insert(relative.to_owned(), entry);
                }
                Mutation::UpsertRegular => {
                    remove_subtree(&mut entries, relative);
                    let mut expected = empty_entry(relative, Kind::File);
                    expected.size = mutation.size;
                    expected.mode = mutation.normalized_mode;
                    entries.insert(relative.to_owned(), expected);
                    changed_files.insert(relative.to_owned(), mutation);
                }
            }
            if entries.len() > root.effective_bounds.maximum_entries {
                bail!("workspace output delta exceeds the admitted entry bound");
            }
        }
        let state = if !exists {
            State::Absent
        } else if entries.is_empty() {
            State::EmptyDirectory
        } else {
            // Validate the prospective tree once before ingest. Re-scanning
            // every retained entry for each changed file would be quadratic.
            validate_bounds(root, &entries, &matcher)?;
            for (relative, mutation) in changed_files {
                if entries
                    .get(&relative)
                    .is_some_and(|entry| entry.kind == Kind::File)
                {
                    let entry = ingest_file(
                        authority,
                        guard,
                        mutation_content,
                        root,
                        &relative,
                        mutation,
                    )?;
                    entries.insert(relative, entry);
                }
            }
            let total_bytes = total_bytes(&entries)?;
            let entries: Vec<Entry> = entries.into_values().collect();
            let value = match root.storage {
                ProductStorage::Content => {
                    let manifest = ExternalContentManifestObject {
                        schema: ryeos_state::objects::EXTERNAL_CONTENT_TREE_SCHEMA.to_owned(),
                        kind: ryeos_state::objects::EXTERNAL_CONTENT_MANIFEST_KIND.to_owned(),
                        entry_count: entries.len(),
                        total_bytes,
                        entries: entries
                            .into_iter()
                            .map(|entry| serde_json::from_value(serde_json::to_value(entry)?))
                            .collect::<Result<_, serde_json::Error>>()?,
                    };
                    manifest.validate()?;
                    serde_json::to_value(manifest)?
                }
                ProductStorage::LargeContent => ExternalLargeContentManifestObject {
                    schema: ryeos_state::objects::EXTERNAL_LARGE_CONTENT_SCHEMA.to_owned(),
                    kind: ryeos_state::objects::EXTERNAL_LARGE_CONTENT_MANIFEST_KIND.to_owned(),
                    entry_count: entries.len(),
                    total_bytes,
                    entries,
                }
                .to_value()?,
            };
            let hash = stage.store_object_admitted(guard, &cas, &value)?;
            State::Captured {
                manifest_kind: value["kind"]
                    .as_str()
                    .context("manifest kind missing")?
                    .to_owned(),
                manifest_hash: hash,
            }
        };
        outputs.insert(root.name.clone(), state);
    }
    mutation_content.ensure_path_binding()?;
    Ok(outputs)
}

fn empty_entry(path: &str, kind: Kind) -> Entry {
    Entry {
        path: path.to_owned(),
        kind,
        mode: None,
        blob_hash: None,
        file_sha256: None,
        size: None,
        chunk_size: None,
        chunk_hashes: Vec::new(),
        target: None,
    }
}

fn load_entries(
    cas: &lillux::CasStore,
    root: &WorkspaceOutputRoot,
    base: Option<&State>,
    matcher: &ryeos_state::ignore::IgnoreMatcher,
) -> anyhow::Result<BTreeMap<String, Entry>> {
    let Some(State::Captured { manifest_hash, .. }) = base else {
        return Ok(BTreeMap::new());
    };
    super::validate_retained_manifest(cas, root, manifest_hash, matcher)?;
    let maximum = match root.storage {
        ProductStorage::Content => ryeos_state::objects::MAX_EXTERNAL_CONTENT_MANIFEST_BYTES,
        ProductStorage::LargeContent => ryeos_state::objects::MAX_LARGE_CONTENT_MANIFEST_BYTES,
    };
    let value = ryeos_state::object_closure::load_exact_cas_object_with_cas(
        cas,
        manifest_hash,
        maximum as u64,
    )?;
    // Large entries are a strict superset of ordinary entries. Conversion is
    // local only; the selected storage tier retains its original wire kind.
    let entries: Vec<Entry> = serde_json::from_value(value["entries"].clone())?;
    Ok(entries
        .into_iter()
        .map(|entry| (entry.path.clone(), entry))
        .collect())
}

fn total_bytes(entries: &BTreeMap<String, Entry>) -> anyhow::Result<u64> {
    entries.values().try_fold(0_u64, |total, entry| {
        total
            .checked_add(entry.size.unwrap_or(0))
            .context("output manifest size overflow")
    })
}

fn validate_bounds(
    root: &WorkspaceOutputRoot,
    entries: &BTreeMap<String, Entry>,
    matcher: &ryeos_state::ignore::IgnoreMatcher,
) -> anyhow::Result<()> {
    super::validate_manifest_entries(
        root,
        entries.len(),
        total_bytes(entries)?,
        entries
            .values()
            .map(|entry| (entry.path.as_str(), entry.kind, entry.size)),
        matcher,
    )
}

fn require_directory_ancestors(
    entries: &BTreeMap<String, Entry>,
    path: &str,
) -> anyhow::Result<()> {
    for (end, _) in path.match_indices('/') {
        if entries
            .get(&path[..end])
            .is_none_or(|entry| entry.kind != Kind::Dir)
        {
            bail!("output mutation has a missing or non-directory parent: {path}");
        }
    }
    Ok(())
}

fn remove_subtree(entries: &mut BTreeMap<String, Entry>, path: &str) {
    entries.remove(path);
    let prefix = format!("{path}/");
    while let Some(key) = entries
        .range(prefix.clone()..)
        .next()
        .filter(|(key, _)| key.starts_with(&prefix))
        .map(|(key, _)| key.clone())
    {
        entries.remove(&key);
    }
}

fn ingest_file(
    authority: &ryeos_state::PinnedStateAuthority,
    guard: &ryeos_state::CasMutationGuard,
    upper: &lillux::PinnedDirectory,
    root: &WorkspaceOutputRoot,
    relative: &str,
    mutation: &WorkspaceMutation,
) -> anyhow::Result<Entry> {
    authority.ensure_guard(guard)?;
    let size = mutation
        .size
        .context("regular output mutation has no size")?;
    let hash = mutation
        .content_hash
        .as_deref()
        .context("regular output mutation has no digest")?;
    let mode = mutation
        .normalized_mode
        .context("regular output mutation has no mode")?;
    let (parent, name) = super::super::workspace::open_mutation_parent(upper, &mutation.path)?;
    let file = parent
        .open_regular(&name, false)?
        .context("frozen output file disappeared")?;
    let before = lillux::observe_open_file_identity(&file)?;
    if before.size() != size || lillux::observe_open_regular_file(&file)?.portable_mode()? != mode {
        bail!(
            "workspace output metadata differs from frozen evidence: {}",
            mutation.path
        );
    }
    let mut entry = empty_entry(relative, Kind::File);
    entry.size = Some(size);
    entry.mode = Some(mode);
    if root.storage == ProductStorage::Content
        || size <= ryeos_state::objects::MAX_EXTERNAL_CONTENT_FILE_BYTES
    {
        let captured = authority.cas_store()?.put_blob_from_open_regular_bounded(
            file.try_clone()?,
            &parent.path().join(&name),
            size,
        )?;
        if captured.hash != hash || captured.size != size || captured.normalized_mode != mode {
            bail!(
                "workspace output bytes differ from frozen evidence: {}",
                mutation.path
            );
        }
        entry.blob_hash = Some(captured.hash);
    } else {
        let captured = authority.large_object_store()?.ingest_open_regular(
            file.try_clone()?,
            ryeos_state::PinnedLargeObjectSourceIdentity {
                containing_device: before.device(),
                inode: before.inode(),
                size,
            },
            &mutation.path,
            Some(hash),
        )?;
        if captured.size != size || captured.file_sha256 != hash {
            bail!("frozen output large-object identity differs");
        }
        entry.file_sha256 = Some(captured.file_sha256);
        entry.chunk_size = Some(captured.chunk_size);
        entry.chunk_hashes = captured.chunk_hashes;
    }
    if lillux::observe_open_file_identity(&file)? != before {
        bail!("workspace output changed while streaming frozen evidence");
    }
    parent.ensure_path_binding()?;
    Ok(entry)
}
