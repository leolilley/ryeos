//! Generic node-private artifact homes for session-owned upstream state.
//!
//! The daemon owns directory identity, modes, creation, and explicit removal.
//! It does not interpret file names or bytes. Integration-specific callers
//! must obtain initial files from admitted signed content before calling this
//! boundary.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use base64::Engine as _;
use ryeos_state::objects::{
    PortableSessionStateClass, PortableSessionStateContract, PortableStateTree,
    PortableStateTreeFile, classify_portable_state_path,
};

const HOME_ROOT: &str = "private-artifact-homes";
const MAX_HOME_ID_BYTES: usize = 128;
const MAX_INITIAL_FILES: usize = 16;
const MAX_INITIAL_FILE_BYTES: usize = 256 * 1024;
const MAX_INITIAL_TOTAL_BYTES: usize = 1024 * 1024;
pub const DEFAULT_MAX_HOME_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const MAX_HOME_ENTRIES: usize = 100_000;
const MAX_HOME_DEPTH: usize = 64;
const HOME_TRAVERSAL_BUDGET: lillux::DirectoryTraversalBudget =
    lillux::DirectoryTraversalBudget::new(MAX_HOME_ENTRIES, MAX_HOME_DEPTH);

fn validate_component(label: &str, value: &str, maximum: usize) -> Result<()> {
    if value.is_empty()
        || value.len() > maximum
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        || value == "."
        || value == ".."
    {
        bail!("{label} is not a bounded portable path component");
    }
    Ok(())
}

pub fn home_path(runtime_state_dir: &Path, home_id: &str) -> Result<PathBuf> {
    validate_component("private artifact home id", home_id, MAX_HOME_ID_BYTES)?;
    Ok(runtime_state_dir.join(HOME_ROOT).join(home_id))
}

pub fn dedicated_session_home_id(session_id: &str) -> Result<String> {
    if session_id.is_empty() || session_id.len() > 256 || session_id.chars().any(char::is_control) {
        bail!("dedicated session id is not canonical and bounded");
    }
    let digest = lillux::cas::sha256_hex(session_id.as_bytes());
    Ok(format!("session-{}", &digest[..32]))
}

pub fn home_id_for_exact_path(runtime_state_dir: &Path, path: &Path) -> Result<String> {
    let root = runtime_state_dir.join(HOME_ROOT);
    if path.parent() != Some(root.as_path()) {
        bail!("private artifact path is outside the owned home root");
    }
    let home_id = path
        .file_name()
        .and_then(OsStr::to_str)
        .ok_or_else(|| anyhow::anyhow!("private artifact path has no UTF-8 home id"))?;
    validate_component("private artifact home id", home_id, MAX_HOME_ID_BYTES)?;
    if home_path(runtime_state_dir, home_id)? != path {
        bail!("private artifact path is not canonical");
    }
    Ok(home_id.to_owned())
}

pub fn create(
    runtime_state_dir: &Path,
    home_id: &str,
    initial_files: &BTreeMap<String, Vec<u8>>,
) -> Result<PathBuf> {
    validate_component("private artifact home id", home_id, MAX_HOME_ID_BYTES)?;
    if initial_files.len() > MAX_INITIAL_FILES {
        bail!("private artifact home has too many initial files");
    }
    let mut total = 0usize;
    for (name, bytes) in initial_files {
        validate_component("private artifact file name", name, 128)?;
        if bytes.len() > MAX_INITIAL_FILE_BYTES {
            bail!("private artifact initial file exceeds its byte ceiling");
        }
        total = total
            .checked_add(bytes.len())
            .context("private artifact initial-file byte total overflow")?;
    }
    if total > MAX_INITIAL_TOTAL_BYTES {
        bail!("private artifact initial files exceed their aggregate byte ceiling");
    }

    let root = lillux::PinnedDirectory::open_or_create(&runtime_state_dir.join(HOME_ROOT))
        .context("open node-private artifact-home root")?;
    root.set_mode(0o700)?;
    let home_name = OsString::from(home_id);
    let home = root
        .create_child(&home_name, 0o700)
        .context("create node-private artifact home")?;
    let result = (|| -> Result<()> {
        for (name, bytes) in initial_files {
            let mut file = home.open_regular_create(OsStr::new(name), true, true, 0o600)?;
            file.write_all(bytes)?;
            file.sync_all()?;
        }
        home.sync()?;
        root.sync()?;
        Ok(())
    })();
    if let Err(error) = result {
        let cleanup = home
            .remove_contents_recursive_bounded(HOME_TRAVERSAL_BUDGET)
            .and_then(|()| {
                root.remove_empty_child_if_same(&home_name, &home)
                    .and_then(|removed| {
                        if removed {
                            Ok(())
                        } else {
                            bail!("private artifact home identity changed during rollback")
                        }
                    })
            });
        return Err(match cleanup {
            Ok(()) => error,
            Err(cleanup) => {
                error.context(format!("private artifact home rollback failed: {cleanup}"))
            }
        });
    }
    Ok(home.path().to_path_buf())
}

pub fn remove(runtime_state_dir: &Path, home_id: &str) -> Result<bool> {
    validate_component("private artifact home id", home_id, MAX_HOME_ID_BYTES)?;
    let Some(root) = lillux::PinnedDirectory::open(&runtime_state_dir.join(HOME_ROOT))? else {
        return Ok(false);
    };
    let name = OsString::from(home_id);
    let Some(home) = root.open_child_directory(&name)? else {
        return Ok(false);
    };
    home.remove_contents_recursive_bounded(HOME_TRAVERSAL_BUDGET)?;
    let removed = root.remove_empty_child_if_same(&name, &home)?;
    if !removed {
        bail!("private artifact home identity changed during removal");
    }
    root.sync()?;
    Ok(true)
}

/// Removes only an empty, exact-identity home left behind before its durable
/// ownership row was committed. Non-empty homes are deliberately preserved:
/// their contents may be the only remaining upstream credential evidence and
/// therefore require explicit operator recovery rather than inference.
pub fn remove_empty_orphan(runtime_state_dir: &Path, home_id: &str) -> Result<bool> {
    validate_component("private artifact home id", home_id, MAX_HOME_ID_BYTES)?;
    let Some(root) = lillux::PinnedDirectory::open(&runtime_state_dir.join(HOME_ROOT))? else {
        return Ok(false);
    };
    let name = OsString::from(home_id);
    let Some(home) = root.open_child_directory(&name)? else {
        return Ok(false);
    };
    if !home.entries_no_follow_bounded(1)?.is_empty() {
        bail!("private artifact orphan is non-empty and requires explicit recovery");
    }
    let removed = root.remove_empty_child_if_same(&name, &home)?;
    if !removed {
        bail!("private artifact home identity changed during orphan recovery");
    }
    root.sync()?;
    Ok(true)
}

pub fn require_within_default_limit(runtime_state_dir: &Path, home_id: &str) -> Result<u64> {
    let path = home_path(runtime_state_dir, home_id)?;
    let home = lillux::PinnedDirectory::open(&path)?
        .ok_or_else(|| anyhow::anyhow!("private artifact home is missing"))?;
    measure_directory_bounded(&home, HOME_TRAVERSAL_BUDGET, DEFAULT_MAX_HOME_BYTES)
}

/// Capture only the workload-declared portable files from one exact private
/// home. The caller must hold the profile-operation lock and prove every
/// workload process using this home has been reaped before entering here.
pub fn capture_portable_state(
    runtime_state_dir: &Path,
    home_id: &str,
    contract: &PortableSessionStateContract,
    upstream_session_id: &str,
) -> Result<PortableStateTree> {
    contract.validate()?;
    let path = home_path(runtime_state_dir, home_id)?;
    let home = lillux::PinnedDirectory::open(&path)?
        .ok_or_else(|| anyhow::anyhow!("private artifact home is missing"))?;
    home.ensure_path_binding()?;
    let traversal_budget = lillux::DirectoryTraversalBudget::new(
        usize::try_from(contract.max_entries).context("portable-state entry ceiling")?,
        usize::from(contract.max_depth),
    );
    home.require_owner_enclosed_tree_bounded(traversal_budget, DEFAULT_MAX_HOME_BYTES)
        .context("validate private artifact home before portable-state capture")?;

    let mut remaining =
        usize::try_from(contract.max_entries).context("portable-state traversal entry ceiling")?;
    let mut selector_counts = BTreeMap::<String, u32>::new();
    let mut files = Vec::new();
    capture_portable_state_directory(
        &home,
        Path::new(""),
        0,
        contract,
        upstream_session_id,
        &mut remaining,
        &mut selector_counts,
        &mut files,
    )?;

    for selector in &contract.selectors {
        let count = selector_counts
            .get(&selector.pattern)
            .copied()
            .unwrap_or_default();
        if count > selector.max_matches {
            bail!(
                "portable-state selector {:?} matched {count} entries; maximum is {}",
                selector.pattern,
                selector.max_matches
            );
        }
        if selector.class == PortableSessionStateClass::PortableSessionState && count != 1 {
            bail!(
                "portable-state selector {:?} must match exactly one file; observed {count}",
                selector.pattern
            );
        }
    }

    files.sort_by(|left, right| (&left.selector, &left.path).cmp(&(&right.selector, &right.path)));
    let tree = PortableStateTree::new(contract, upstream_session_id, files)?;
    home.require_owner_enclosed_tree_bounded(traversal_budget, DEFAULT_MAX_HOME_BYTES)
        .context("validate private artifact home after portable-state capture")?;
    home.ensure_path_binding()?;
    Ok(tree)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortableStateInstallOutcome {
    AlreadyCurrent,
    Advanced,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PortableTargetFileState {
    Absent,
    Predecessor,
    Incoming,
}

/// Conditionally install one already-validated portable session tree into an
/// exact node-private profile home. The caller owns the durable job/retry
/// record, holds the profile-operation lock, and has proved no worker using
/// this home can run. This operation is deliberately idempotent: a crash may
/// leave some files at the incoming value, but a retry accepts only absent,
/// exact predecessor, or exact incoming bytes before completing the advance.
pub fn install_portable_state_conditionally(
    runtime_state_dir: &Path,
    home_id: &str,
    contract: &PortableSessionStateContract,
    upstream_session_id: &str,
    expected_predecessor: Option<&PortableStateTree>,
    incoming: &PortableStateTree,
) -> Result<PortableStateInstallOutcome> {
    incoming.validate(contract, upstream_session_id)?;
    if let Some(predecessor) = expected_predecessor {
        predecessor.validate(contract, upstream_session_id)?;
        let predecessor_paths = predecessor
            .files
            .iter()
            .map(|file| (&file.selector, &file.path))
            .collect::<Vec<_>>();
        let incoming_paths = incoming
            .files
            .iter()
            .map(|file| (&file.selector, &file.path))
            .collect::<Vec<_>>();
        if predecessor_paths != incoming_paths {
            bail!(
                "portable-state advance changes its selected path set; this profile requires a native transactional restore contract"
            );
        }
    }

    let path = home_path(runtime_state_dir, home_id)?;
    let home = lillux::PinnedDirectory::open(&path)?
        .ok_or_else(|| anyhow::anyhow!("private artifact home is missing"))?;
    home.ensure_path_binding()?;
    let traversal_budget = lillux::DirectoryTraversalBudget::new(
        usize::try_from(contract.max_entries).context("portable-state entry ceiling")?,
        usize::from(contract.max_depth),
    );
    home.require_owner_enclosed_tree_bounded(traversal_budget, DEFAULT_MAX_HOME_BYTES)
        .context("validate private artifact home before portable-state restore")?;

    // Lillux recovery metadata is substrate state, not workload state. Finish
    // an interrupted exact replacement for every admitted target before the
    // strict classifier sees the directory. The durable caller retries this
    // same incoming tree, so recovery can only expose its old or new bytes.
    for incoming_file in &incoming.files {
        let Some((parent, name)) = open_existing_portable_parent(&home, &incoming_file.path)?
        else {
            continue;
        };
        if let Err(error) = parent.recover_conditional_byte_replacement_atomic(&name) {
            if error.namespace_committed() {
                parent.sync().map_err(|sync_error| {
                    anyhow::anyhow!(
                        "portable-state recovery committed but durability remains uncertain; retry the same durable restore job: {error}; {sync_error:#}"
                    )
                })?;
            } else {
                return Err(anyhow::anyhow!(error)).with_context(|| {
                    format!(
                        "recover interrupted portable-state install {:?}",
                        incoming_file.path
                    )
                });
            }
        }
    }

    // Scan the complete current namespace through the same classifier used by
    // capture. Unlike capture, absence of the portable selector is legal here.
    let mut remaining =
        usize::try_from(contract.max_entries).context("portable-state traversal entry ceiling")?;
    let mut selector_counts = BTreeMap::<String, u32>::new();
    let mut current_files = Vec::new();
    capture_portable_state_directory(
        &home,
        Path::new(""),
        0,
        contract,
        upstream_session_id,
        &mut remaining,
        &mut selector_counts,
        &mut current_files,
    )?;
    let current_by_path = current_files
        .iter()
        .map(|file| (file.path.as_str(), file))
        .collect::<BTreeMap<_, _>>();
    let incoming_by_path = incoming
        .files
        .iter()
        .map(|file| (file.path.as_str(), file))
        .collect::<BTreeMap<_, _>>();
    let predecessor_by_path = expected_predecessor
        .map(|tree| {
            tree.files
                .iter()
                .map(|file| (file.path.as_str(), file))
                .collect::<BTreeMap<_, _>>()
        })
        .unwrap_or_default();

    for path in current_by_path.keys() {
        if !incoming_by_path.contains_key(path) {
            bail!("target home contains portable state outside the incoming selected path set");
        }
    }

    let mut planned = Vec::with_capacity(incoming.files.len());
    for incoming_file in &incoming.files {
        let state = match current_by_path.get(incoming_file.path.as_str()) {
            None => PortableTargetFileState::Absent,
            Some(current) if same_portable_file_content(current, incoming_file) => {
                PortableTargetFileState::Incoming
            }
            Some(current)
                if predecessor_by_path
                    .get(incoming_file.path.as_str())
                    .is_some_and(|predecessor| {
                        same_portable_file_content(current, predecessor)
                    }) =>
            {
                PortableTargetFileState::Predecessor
            }
            Some(_) => {
                bail!(
                    "target portable file {:?} matches neither the exact predecessor nor incoming checkpoint",
                    incoming_file.path
                )
            }
        };
        planned.push((incoming_file, state));
    }

    let already_current = planned
        .iter()
        .all(|(_, state)| *state == PortableTargetFileState::Incoming);
    for (incoming_file, target_state) in planned {
        if target_state == PortableTargetFileState::Incoming {
            continue;
        }
        let (parent, name) = open_portable_parent(&home, &incoming_file.path, true)?;
        let incoming_bytes = PortableStateTree::content_bytes(incoming_file)?;
        let result = match target_state {
            PortableTargetFileState::Absent => parent.replace_bytes_if_matches_atomic(
                &name,
                None,
                |_| Ok(()),
                &incoming_bytes,
                0o600,
            ),
            PortableTargetFileState::Predecessor => {
                let predecessor = predecessor_by_path
                    .get(incoming_file.path.as_str())
                    .ok_or_else(|| anyhow::anyhow!("portable predecessor disappeared"))?;
                let incumbent = parent
                    .open_regular(&name, false)?
                    .ok_or_else(|| anyhow::anyhow!("portable predecessor disappeared"))?;
                let observation = lillux::observe_open_regular_file(&incumbent)?;
                let expected_digest = predecessor.content_sha256.clone();
                let max_file_bytes = contract.max_file_bytes;
                parent.replace_bytes_if_matches_atomic(
                    &name,
                    Some(&incumbent),
                    move |current| {
                        let current_observation = lillux::observe_open_regular_file(current)?;
                        if !current_observation.matches_quarantined_incumbent(&observation) {
                            bail!("portable predecessor identity changed before replacement");
                        }
                        let mut current = current.try_clone()?;
                        let bytes = lillux::read_open_regular_file_stable_bounded(
                            &mut current,
                            &current_observation,
                            max_file_bytes,
                        )?;
                        if lillux::sha256_hex(&bytes) != expected_digest {
                            bail!("portable predecessor bytes changed before replacement");
                        }
                        Ok(())
                    },
                    &incoming_bytes,
                    0o600,
                )
            }
            PortableTargetFileState::Incoming => unreachable!(),
        };
        if let Err(error) = result {
            if error.namespace_committed() {
                parent.sync().map_err(|sync_error| {
                    anyhow::anyhow!(
                        "portable state was committed but durability remains uncertain; retry the same durable restore job: {error}; {sync_error:#}"
                    )
                })?;
            } else {
                return Err(anyhow::anyhow!(error))
                    .with_context(|| format!("conditionally install {:?}", incoming_file.path));
            }
        }
    }

    let installed =
        capture_portable_state(runtime_state_dir, home_id, contract, upstream_session_id)?;
    if &installed != incoming {
        bail!("portable state differs after conditional installation");
    }
    home.ensure_path_binding()?;
    Ok(if already_current {
        PortableStateInstallOutcome::AlreadyCurrent
    } else {
        PortableStateInstallOutcome::Advanced
    })
}

fn same_portable_file_content(left: &PortableStateTreeFile, right: &PortableStateTreeFile) -> bool {
    left.selector == right.selector
        && left.path == right.path
        && left.size_bytes == right.size_bytes
        && left.content_sha256 == right.content_sha256
        && left.content_base64 == right.content_base64
}

fn open_portable_parent(
    home: &lillux::PinnedDirectory,
    relative_file: &str,
    create: bool,
) -> Result<(lillux::PinnedDirectory, OsString)> {
    let mut components = relative_file.split('/').collect::<Vec<_>>();
    let file_name = components
        .pop()
        .filter(|name| !name.is_empty())
        .ok_or_else(|| anyhow::anyhow!("portable-state path has no file name"))?;
    let mut parent = home.try_clone()?;
    for component in components {
        parent = if create {
            parent.open_or_create_child(OsStr::new(component), 0o700)?
        } else {
            parent
                .open_child_directory(OsStr::new(component))?
                .ok_or_else(|| anyhow::anyhow!("portable-state parent is absent"))?
        };
    }
    Ok((parent, OsString::from(file_name)))
}

fn open_existing_portable_parent(
    home: &lillux::PinnedDirectory,
    relative_file: &str,
) -> Result<Option<(lillux::PinnedDirectory, OsString)>> {
    let mut components = relative_file.split('/').collect::<Vec<_>>();
    let file_name = components
        .pop()
        .filter(|name| !name.is_empty())
        .ok_or_else(|| anyhow::anyhow!("portable-state path has no file name"))?;
    let mut parent = home.try_clone()?;
    for component in components {
        let Some(child) = parent.open_child_directory(OsStr::new(component))? else {
            return Ok(None);
        };
        parent = child;
    }
    Ok(Some((parent, OsString::from(file_name))))
}

#[allow(clippy::too_many_arguments)]
fn capture_portable_state_directory(
    directory: &lillux::PinnedDirectory,
    relative: &Path,
    depth: usize,
    contract: &PortableSessionStateContract,
    upstream_session_id: &str,
    remaining: &mut usize,
    selector_counts: &mut BTreeMap<String, u32>,
    files: &mut Vec<PortableStateTreeFile>,
) -> Result<()> {
    if depth > usize::from(contract.max_depth) {
        bail!("portable-state traversal reached its directory-depth ceiling");
    }
    let entries = directory.entries_no_follow_bounded(*remaining)?;
    *remaining = remaining
        .checked_sub(entries.len())
        .ok_or_else(|| anyhow::anyhow!("portable-state traversal budget underflow"))?;
    for entry in entries {
        let name = entry
            .name
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("portable-state path is not UTF-8"))?;
        let child_relative = relative.join(name);
        let child_path = child_relative
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("portable-state path is not UTF-8"))?;
        let selector = classify_portable_state_path(contract, child_path, upstream_session_id)?;
        let count = selector_counts.entry(selector.pattern.clone()).or_default();
        *count = count
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("portable-state selector count overflow"))?;
        if *count > selector.max_matches {
            bail!(
                "portable-state selector {:?} exceeds its match ceiling",
                selector.pattern
            );
        }

        match entry.entry_type {
            lillux::PinnedEntryType::Directory => {
                if selector.class == PortableSessionStateClass::PortableSessionState {
                    bail!("portable-session state selector resolved to a directory");
                }
                let child = directory
                    .open_child_directory(&entry.name)?
                    .ok_or_else(|| anyhow::anyhow!("portable-state directory disappeared"))?;
                directory.ensure_entry_observation(&entry)?;
                capture_portable_state_directory(
                    &child,
                    &child_relative,
                    depth + 1,
                    contract,
                    upstream_session_id,
                    remaining,
                    selector_counts,
                    files,
                )?;
                directory.ensure_entry_observation(&entry)?;
            }
            lillux::PinnedEntryType::Regular => {
                if selector.class != PortableSessionStateClass::PortableSessionState {
                    directory.ensure_entry_observation(&entry)?;
                    continue;
                }
                let mut file = directory
                    .open_regular(&entry.name, false)?
                    .ok_or_else(|| anyhow::anyhow!("portable-state file disappeared"))?;
                let observation = lillux::observe_open_regular_file(&file)?;
                if !observation.matches_directory_entry(&entry) {
                    bail!("portable-state file changed identity before capture");
                }
                let bytes = lillux::read_open_regular_file_stable_bounded(
                    &mut file,
                    &observation,
                    contract.max_file_bytes,
                )?;
                directory.ensure_entry_observation(&entry)?;
                files.push(PortableStateTreeFile {
                    selector: selector.pattern.clone(),
                    path: child_path.to_string(),
                    size_bytes: bytes.len() as u64,
                    content_sha256: lillux::sha256_hex(&bytes),
                    content_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
                });
            }
            lillux::PinnedEntryType::Symlink
                if selector.class == PortableSessionStateClass::RebuildableCache =>
            {
                // Rebuildable namespaces are deliberately absent from the
                // portable checkpoint. A workload may use local symlinks in
                // such a cache; observe the link identity without following
                // it and discard it from the transfer contract.
                directory.ensure_entry_observation(&entry)?;
            }
            lillux::PinnedEntryType::Symlink
            | lillux::PinnedEntryType::CharacterDevice
            | lillux::PinnedEntryType::BlockDevice
            | lillux::PinnedEntryType::Fifo
            | lillux::PinnedEntryType::Socket
            | lillux::PinnedEntryType::Other => {
                bail!("portable-state capture refuses links and special entries")
            }
        }
    }
    Ok(())
}

fn measure_directory_bounded(
    root: &lillux::PinnedDirectory,
    budget: lillux::DirectoryTraversalBudget,
    maximum_bytes: u64,
) -> Result<u64> {
    root.require_owner_enclosed_tree_bounded(budget, maximum_bytes)
        .context("validate private artifact home through Lillux")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;

    fn portable_contract() -> PortableSessionStateContract {
        PortableSessionStateContract {
            schema: 1,
            restore_contract: "ryeos.worker_session.restore.v1".to_string(),
            max_depth: 8,
            max_entries: 32,
            max_file_bytes: 1024,
            max_total_bytes: 2048,
            selectors: vec![
                ryeos_state::objects::PortableSessionStateSelector {
                    pattern: "auth.json".to_string(),
                    class: PortableSessionStateClass::NodePrivateCredentialState,
                    max_matches: 1,
                },
                ryeos_state::objects::PortableSessionStateSelector {
                    pattern: "cache/**".to_string(),
                    class: PortableSessionStateClass::RebuildableCache,
                    max_matches: 32,
                },
                ryeos_state::objects::PortableSessionStateSelector {
                    pattern: "config.toml".to_string(),
                    class: PortableSessionStateClass::ForbiddenOrUnknown,
                    max_matches: 1,
                },
                ryeos_state::objects::PortableSessionStateSelector {
                    pattern: "sessions/**".to_string(),
                    class: PortableSessionStateClass::ForbiddenOrUnknown,
                    max_matches: 32,
                },
                ryeos_state::objects::PortableSessionStateSelector {
                    pattern: "sessions/*/rollout-*-{session_id}.jsonl".to_string(),
                    class: PortableSessionStateClass::PortableSessionState,
                    max_matches: 1,
                },
            ],
        }
    }

    #[test]
    fn creates_opaque_private_files_and_removes_exact_home() {
        let tmp = tempfile::tempdir().unwrap();
        let files = BTreeMap::from([("fixture.conf".to_owned(), b"fixture".to_vec())]);
        let path = create(tmp.path(), "home-one", &files).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(path.join("fixture.conf"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::read(path.join("fixture.conf")).unwrap(),
            b"fixture"
        );
        assert!(remove(tmp.path(), "home-one").unwrap());
        assert!(!path.exists());
    }

    #[test]
    fn captures_only_the_exact_session_file_and_refuses_unknown_state() {
        let tmp = tempfile::tempdir().unwrap();
        let files = BTreeMap::from([
            ("auth.json".to_owned(), b"secret".to_vec()),
            ("config.toml".to_owned(), b"baseline".to_vec()),
        ]);
        let home = create(tmp.path(), "portable-home", &files).unwrap();
        std::fs::create_dir_all(home.join("sessions/day")).unwrap();
        std::fs::write(
            home.join("sessions/day/rollout-a-session-one.jsonl"),
            b"portable",
        )
        .unwrap();
        std::fs::write(
            home.join("sessions/day/rollout-a-session-two.jsonl"),
            b"unrelated",
        )
        .unwrap();
        std::fs::create_dir(home.join("cache")).unwrap();
        std::os::unix::fs::symlink(
            tmp.path().join("outside-cache-target"),
            home.join("cache/workload-link"),
        )
        .unwrap();

        let tree = capture_portable_state(
            tmp.path(),
            "portable-home",
            &portable_contract(),
            "session-one",
        )
        .unwrap();
        assert_eq!(tree.files.len(), 1);
        assert_eq!(
            PortableStateTree::content_bytes(&tree.files[0]).unwrap(),
            b"portable"
        );
        assert!(
            !tree
                .canonical_bytes()
                .unwrap()
                .windows(6)
                .any(|part| part == b"secret")
        );

        std::os::unix::fs::symlink(
            tmp.path().join("outside-state-target"),
            home.join("sessions/day/unselected-link"),
        )
        .unwrap();
        assert!(
            capture_portable_state(
                tmp.path(),
                "portable-home",
                &portable_contract(),
                "session-one"
            )
            .is_err()
        );
        std::fs::remove_file(home.join("sessions/day/unselected-link")).unwrap();

        std::fs::write(home.join("unknown"), b"not classified").unwrap();
        assert!(
            capture_portable_state(
                tmp.path(),
                "portable-home",
                &portable_contract(),
                "session-one"
            )
            .is_err()
        );
    }

    fn write_session_file(home: &Path, session_id: &str, bytes: &[u8]) {
        std::fs::create_dir_all(home.join("sessions/day")).unwrap();
        std::fs::write(
            home.join(format!("sessions/day/rollout-a-{session_id}.jsonl")),
            bytes,
        )
        .unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn conditionally_installs_exact_portable_state_and_preserves_unselected_bytes() {
        let tmp = tempfile::tempdir().unwrap();
        let initial = BTreeMap::from([
            ("auth.json".to_owned(), b"source-secret".to_vec()),
            ("config.toml".to_owned(), b"source-config".to_vec()),
        ]);
        let source = create(tmp.path(), "source-home", &initial).unwrap();
        write_session_file(&source, "session-one", b"predecessor");
        let predecessor = capture_portable_state(
            tmp.path(),
            "source-home",
            &portable_contract(),
            "session-one",
        )
        .unwrap();
        write_session_file(&source, "session-one", b"incoming");
        let incoming = capture_portable_state(
            tmp.path(),
            "source-home",
            &portable_contract(),
            "session-one",
        )
        .unwrap();

        let target_initial = BTreeMap::from([
            ("auth.json".to_owned(), b"target-secret".to_vec()),
            ("config.toml".to_owned(), b"target-config".to_vec()),
        ]);
        let target = create(tmp.path(), "target-home", &target_initial).unwrap();
        write_session_file(&target, "session-one", b"predecessor");
        write_session_file(&target, "session-two", b"unrelated");

        assert_eq!(
            install_portable_state_conditionally(
                tmp.path(),
                "target-home",
                &portable_contract(),
                "session-one",
                Some(&predecessor),
                &incoming,
            )
            .unwrap(),
            PortableStateInstallOutcome::Advanced
        );
        assert_eq!(
            capture_portable_state(
                tmp.path(),
                "target-home",
                &portable_contract(),
                "session-one"
            )
            .unwrap(),
            incoming
        );
        assert_eq!(
            std::fs::read(target.join("auth.json")).unwrap(),
            b"target-secret"
        );
        assert_eq!(
            std::fs::read(target.join("config.toml")).unwrap(),
            b"target-config"
        );
        assert_eq!(
            std::fs::read(target.join("sessions/day/rollout-a-session-two.jsonl")).unwrap(),
            b"unrelated"
        );
        assert_eq!(
            install_portable_state_conditionally(
                tmp.path(),
                "target-home",
                &portable_contract(),
                "session-one",
                Some(&predecessor),
                &incoming,
            )
            .unwrap(),
            PortableStateInstallOutcome::AlreadyCurrent
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn portable_install_accepts_absence_and_refuses_conflict_before_mutation() {
        let tmp = tempfile::tempdir().unwrap();
        let initial = BTreeMap::from([
            ("auth.json".to_owned(), b"source-secret".to_vec()),
            ("config.toml".to_owned(), b"source-config".to_vec()),
        ]);
        let source = create(tmp.path(), "source-home", &initial).unwrap();
        write_session_file(&source, "session-one", b"incoming");
        let incoming = capture_portable_state(
            tmp.path(),
            "source-home",
            &portable_contract(),
            "session-one",
        )
        .unwrap();

        let target = create(tmp.path(), "absent-home", &initial).unwrap();
        write_session_file(&target, "session-two", b"unrelated");
        assert_eq!(
            install_portable_state_conditionally(
                tmp.path(),
                "absent-home",
                &portable_contract(),
                "session-one",
                None,
                &incoming,
            )
            .unwrap(),
            PortableStateInstallOutcome::Advanced
        );
        assert_eq!(
            std::fs::read(target.join("sessions/day/rollout-a-session-two.jsonl")).unwrap(),
            b"unrelated"
        );

        let conflict = create(tmp.path(), "conflict-home", &initial).unwrap();
        write_session_file(&conflict, "session-one", b"local-conflict");
        write_session_file(&conflict, "session-two", b"unrelated");
        let before =
            std::fs::read(conflict.join("sessions/day/rollout-a-session-one.jsonl")).unwrap();
        assert!(
            install_portable_state_conditionally(
                tmp.path(),
                "conflict-home",
                &portable_contract(),
                "session-one",
                None,
                &incoming,
            )
            .is_err()
        );
        assert_eq!(
            std::fs::read(conflict.join("sessions/day/rollout-a-session-one.jsonl")).unwrap(),
            before
        );
        assert_eq!(
            std::fs::read(conflict.join("sessions/day/rollout-a-session-two.jsonl")).unwrap(),
            b"unrelated"
        );
    }

    #[test]
    fn rejects_path_components_and_existing_home() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(create(tmp.path(), "../escape", &BTreeMap::new()).is_err());
        create(tmp.path(), "home-one", &BTreeMap::new()).unwrap();
        assert!(create(tmp.path(), "home-one", &BTreeMap::new()).is_err());
    }

    #[test]
    fn recovers_only_empty_creation_orphans() {
        let tmp = tempfile::tempdir().unwrap();
        let empty = create(tmp.path(), "empty-orphan", &BTreeMap::new()).unwrap();
        assert!(remove_empty_orphan(tmp.path(), "empty-orphan").unwrap());
        assert!(!empty.exists());

        let files = BTreeMap::from([("credential.json".to_owned(), b"opaque".to_vec())]);
        let nonempty = create(tmp.path(), "nonempty-orphan", &files).unwrap();
        assert!(remove_empty_orphan(tmp.path(), "nonempty-orphan").is_err());
        assert!(nonempty.exists());
    }

    #[test]
    fn measures_only_regular_no_follow_home_content() {
        let tmp = tempfile::tempdir().unwrap();
        let files = BTreeMap::from([("fixture.conf".to_owned(), b"fixture".to_vec())]);
        let path = create(tmp.path(), "home-one", &files).unwrap();
        let nested = path.join("nested");
        std::fs::create_dir(&nested).unwrap();
        std::fs::set_permissions(&nested, std::fs::Permissions::from_mode(0o700)).unwrap();
        let state = nested.join("state");
        std::fs::write(&state, b"state").unwrap();
        // Nested workload modes are opaque. The exact 0700 home root is the
        // privacy boundary; a non-secret workload file may legitimately be
        // world-readable inside that untraversable root.
        std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(
            require_within_default_limit(tmp.path(), "home-one").unwrap(),
            12
        );
        let outside = tmp.path().join("outside");
        std::fs::write(&outside, b"outside-is-not-counted").unwrap();
        std::fs::set_permissions(&outside, std::fs::Permissions::from_mode(0o644)).unwrap();
        std::os::unix::fs::symlink("../outside", path.join("link")).unwrap();
        assert_eq!(
            require_within_default_limit(tmp.path(), "home-one").unwrap(),
            12
        );
        assert_eq!(
            std::fs::metadata(&outside).unwrap().permissions().mode() & 0o777,
            0o644
        );
    }

    #[test]
    fn rejects_a_home_root_that_is_not_exactly_owner_private() {
        let tmp = tempfile::tempdir().unwrap();
        let path = create(tmp.path(), "home-one", &BTreeMap::new()).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o750)).unwrap();
        assert!(require_within_default_limit(tmp.path(), "home-one").is_err());
    }

    #[test]
    fn bounded_home_walk_rejects_namespace_before_overallocation() {
        let tmp = tempfile::tempdir().unwrap();
        let path = create(tmp.path(), "home-one", &BTreeMap::new()).unwrap();
        for name in ["one", "two"] {
            let file = path.join(name);
            std::fs::write(&file, b"x").unwrap();
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let home = lillux::PinnedDirectory::open(&path).unwrap().unwrap();
        assert!(
            measure_directory_bounded(
                &home,
                lillux::DirectoryTraversalBudget::new(1, MAX_HOME_DEPTH),
                DEFAULT_MAX_HOME_BYTES,
            )
            .is_err()
        );
    }

    #[test]
    fn bounded_home_walk_and_removal_reject_excessive_depth() {
        let tmp = tempfile::tempdir().unwrap();
        let path = create(tmp.path(), "home-one", &BTreeMap::new()).unwrap();
        let first = path.join("first");
        let second = first.join("second");
        std::fs::create_dir(&first).unwrap();
        std::fs::create_dir(&second).unwrap();
        for directory in [&first, &second] {
            std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let home = lillux::PinnedDirectory::open(&path).unwrap().unwrap();
        assert!(
            measure_directory_bounded(
                &home,
                lillux::DirectoryTraversalBudget::new(MAX_HOME_ENTRIES, 1),
                DEFAULT_MAX_HOME_BYTES,
            )
            .is_err()
        );

        let mut current = second;
        for index in 0..=MAX_HOME_DEPTH {
            current = current.join(format!("depth-{index}"));
            std::fs::create_dir(&current).unwrap();
            std::fs::set_permissions(&current, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        assert!(remove(tmp.path(), "home-one").is_err());
        assert!(path.exists());
    }
}
