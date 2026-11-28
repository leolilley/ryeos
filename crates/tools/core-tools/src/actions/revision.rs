//! Operator-side revision records and removal.
//!
//! A change the operator makes in their own process (`ryeos sign`,
//! `ryeos item remove`) is recorded in the item's revision chain, signed by the
//! operator's key. The daemon never holds that key, so these records are
//! written here, in the operator's process, whether or not the node is up.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use lillux::crypto::SigningKey;
use ryeos_engine::canonical_ref::CanonicalRef;
use ryeos_node_runtime::item_revision::{
    self, ActorRole, ChainItem, Change, REVISION_KIND, Recorded,
};
use serde::Serialize;

/// Record one operator change to the item at `file_path`: `after` is the file
/// as the change left it, `None` for a removal.
pub(crate) fn record_operator_change(
    project_root: &Path,
    signing_key: &SigningKey,
    item_ref: &str,
    file_path: &Path,
    before: Option<&[u8]>,
    after: Option<&[u8]>,
    action: &str,
) -> Result<Recorded> {
    let item = ChainItem::parse(item_ref)?;
    let ext = file_path
        .extension()
        .and_then(OsStr::to_str)
        .map(|ext| format!(".{ext}"))
        .ok_or_else(|| anyhow!("item file {} has no extension", file_path.display()))?;
    let after = after
        .map(|bytes| std::str::from_utf8(bytes).context("signed item is not UTF-8"))
        .transpose()?;
    item_revision::append(
        project_root,
        signing_key,
        ActorRole::Operator,
        Change {
            item: &item,
            file_ext: &ext,
            before,
            after,
            provenance: BTreeMap::from([("action".to_owned(), action.to_owned())]),
        },
    )
    .with_context(|| {
        format!(
            "`{item_ref}` changed but its revision could not be recorded; the change reads as \
             unrecorded until it is"
        )
    })
}

/// Refuse to sign or remove a revision record: the chain is append-only and
/// each record is signed by the key that made the change it records.
pub(crate) fn refuse_revision_kind(kind: &str) -> Result<()> {
    if kind == REVISION_KIND {
        bail!(
            "revision records are written only by the change they record; they cannot be signed \
             or removed by hand"
        );
    }
    Ok(())
}

#[derive(Debug, Serialize)]
pub struct ItemRemoveReport {
    pub item_ref: String,
    pub path: String,
    pub revision: u64,
    pub revision_digest: String,
}

/// Remove one project item as the operator: the file goes, and a tombstone
/// revision signed with the operator's key is recorded. The operator may
/// remove any project item, including one a run authored.
pub fn run_item_remove(
    app_root: &Path,
    item_ref: &str,
    project_path: &Path,
) -> Result<ItemRemoveReport> {
    let authoring = super::sign::load_offline_authoring(app_root)?;
    remove_item(
        &authoring.kinds,
        &authoring.signing_key,
        item_ref,
        project_path,
    )
}

/// [`run_item_remove`] over an already-admitted kind registry and the
/// operator key the caller holds.
pub fn remove_item(
    kinds: &ryeos_engine::kind_registry::KindRegistry,
    signing_key: &SigningKey,
    item_ref: &str,
    project_path: &Path,
) -> Result<ItemRemoveReport> {
    let canonical = CanonicalRef::parse(item_ref)
        .map_err(|error| anyhow!("malformed canonical ref `{item_ref}`: {error}"))?;
    if canonical.suffix.is_some() {
        bail!("malformed canonical ref `{item_ref}`: remove refs do not support suffixes");
    }
    refuse_revision_kind(&canonical.kind)?;
    let kind_schema = kinds.get(&canonical.kind).ok_or_else(|| {
        anyhow!(
            "unknown kind `{}` — no kind schema registered",
            canonical.kind
        )
    })?;
    let ai_root = project_path.join(ryeos_engine::AI_DIR);
    let kind_dir = ai_root.join(&kind_schema.directory);

    let project = lillux::PinnedDirectory::open(project_path)?
        .ok_or_else(|| anyhow!("project root is unavailable: {}", project_path.display()))?;
    let _project_lock = project.lock_exclusive()?;

    let mut found: Vec<PathBuf> = kind_schema
        .extensions
        .iter()
        .map(|spec| kind_dir.join(format!("{}{}", canonical.bare_id, spec.ext)))
        .filter(|candidate| candidate.is_file())
        .collect();
    let file_path = match found.len() {
        0 => bail!(
            "item `{item_ref}` not found in project (searched {} with extensions {:?})",
            kind_dir.display(),
            kind_schema.extension_strs()
        ),
        1 => found.remove(0),
        _ => bail!("ambiguous duplicate files for `{item_ref}`"),
    };
    if super::runtime_owned::is_runtime_owned_file(&file_path, &ai_root) {
        bail!(
            "runtime-owned path is not a removable item: {}",
            file_path.display()
        );
    }
    if file_path
        .strip_prefix(&kind_dir)
        .is_ok_and(|relative| kind_schema.excludes_relative_path(relative))
    {
        bail!(
            "excluded auxiliary source is not an independently removable item: {}",
            file_path.display()
        );
    }

    lillux::atomic_fs::require_name_within_quarantine_cap(&file_path)
        .with_context(|| format!("item `{item_ref}` cannot be removed"))?;
    let parent_path = file_path
        .parent()
        .ok_or_else(|| anyhow!("item file has no parent directory"))?;
    let name = file_path
        .file_name()
        .ok_or_else(|| anyhow!("item file has no name"))?;
    let parent = lillux::PinnedDirectory::open(parent_path)?
        .ok_or_else(|| anyhow!("item directory is unavailable"))?;
    let incumbent = parent
        .open_regular(name, false)?
        .ok_or_else(|| anyhow!("item `{item_ref}` is not a regular file"))?;
    let observation = lillux::observe_open_regular_file(&incumbent)?;
    let before = lillux::read_open_regular_file_stable_bounded(
        &incumbent,
        &observation,
        ryeos_engine::item_resolution::MAX_ITEM_SOURCE_BYTES,
    )?;
    let expected = before.clone();
    parent.ensure_path_binding()?;
    let removed = parent.remove_if_same_validated_atomic(name, &incumbent, |current| {
        let current_observation = lillux::observe_open_regular_file(current)?;
        if !current_observation.matches_quarantined_incumbent(&observation) {
            bail!("item metadata changed before removal");
        }
        let current = current.try_clone()?;
        let bytes = lillux::read_open_regular_file_stable_bounded(
            &current,
            &current_observation,
            ryeos_engine::item_resolution::MAX_ITEM_SOURCE_BYTES,
        )?;
        if bytes != expected {
            bail!("item bytes changed before removal");
        }
        Ok(())
    });
    match removed {
        Ok(()) => {}
        Err(error) if error.namespace_committed() => {
            parent.sync().map_err(|sync_error| {
                anyhow!(
                    "item was removed, but durability remains uncertain after retrying the \
                     parent barrier: {error}; {sync_error:#}"
                )
            })?;
        }
        Err(error) => {
            return Err(anyhow!(error))
                .with_context(|| format!("atomically remove {}", file_path.display()));
        }
    }
    let recorded = record_operator_change(
        project_path,
        signing_key,
        &canonical.to_string(),
        &file_path,
        Some(before.as_slice()),
        None,
        "remove",
    )?;
    Ok(ItemRemoveReport {
        item_ref: canonical.to_string(),
        path: file_path.display().to_string(),
        revision: recorded.seq,
        revision_digest: recorded.digest,
    })
}
