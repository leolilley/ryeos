//! Live project registrations: which checkouts an owner has registered under
//! a project name on this node.
//!
//! A registration is a node-signed record
//! `<app_root>/.ai/node/projects/<owner-hex>/<name>.yaml` listing the
//! canonical checkout paths the owner registered under that name. It is
//! written only by the `node.project.register` / `node.project.unregister`
//! offline operations, which the node's own operator signs on a stopped node.
//!
//! Registrations decide nothing yet: the only reader is `ryeos node doctor`.
//! At the owner+name trust cut they become the condition for a live checkout
//! to state its project identity, so registrations made before the cut become
//! trust at the cut.

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use ryeos_node_runtime::identity::NodeIdentity;
use ryeos_state::project_identity::{ProjectDeclaration, ProjectIdentity, ProjectName};
use serde::{Deserialize, Serialize};

/// Schema of a registration record body. Final: the record shape is fixed
/// before anything reads it as trust.
pub const PROJECT_REGISTRATION_SCHEMA: u32 = 1;

/// Directory under `.ai/node` holding one directory per owner.
pub const PROJECTS_DIR: &str = "projects";

/// Most checkout paths one registration may list.
pub const MAX_REGISTERED_PATHS: usize = 64;

const MAX_OWNERS: usize = 256;
const MAX_NAMES_PER_OWNER: usize = 1024;
const MAX_PATH_BYTES: usize = 4096;

/// The signed body of one registration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectRegistration {
    pub schema: u32,
    /// `fp:<64 lowercase hex>/<name>`.
    pub identity: ProjectIdentity,
    /// Canonical absolute checkout paths, sorted and unique.
    pub paths: Vec<PathBuf>,
    /// Digest of the offline request that last wrote this record.
    pub request_digest: String,
}

impl ProjectRegistration {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.schema == PROJECT_REGISTRATION_SCHEMA,
            "project registration schema {} is not supported (expected {PROJECT_REGISTRATION_SCHEMA})",
            self.schema
        );
        ensure!(
            !self.paths.is_empty() && self.paths.len() <= MAX_REGISTERED_PATHS,
            "a project registration lists 1 to {MAX_REGISTERED_PATHS} paths"
        );
        let mut previous: Option<&Path> = None;
        for path in &self.paths {
            validate_checkout_path(path)?;
            if let Some(previous) = previous {
                ensure!(
                    previous < path.as_path(),
                    "project registration paths must be sorted and unique"
                );
            }
            previous = Some(path);
        }
        ensure!(
            self.request_digest.len() == 64
                && self
                    .request_digest
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
            "project registration request digest must be 64 lowercase hex"
        );
        Ok(())
    }
}

fn validate_checkout_path(path: &Path) -> Result<()> {
    let text = path
        .to_str()
        .with_context(|| format!("checkout path {} is not UTF-8", path.display()))?;
    ensure!(
        path.is_absolute()
            && !text.is_empty()
            && text.len() <= MAX_PATH_BYTES
            && !text.contains(['\0', '\n', '\r'])
            && path.components().all(|part| matches!(
                part,
                std::path::Component::RootDir | std::path::Component::Normal(_)
            )),
        "checkout path must be a normalized absolute path: {}",
        path.display()
    );
    Ok(())
}

/// `<app_root>/.ai/node/projects`.
pub fn projects_dir(app_root: &Path) -> PathBuf {
    app_root
        .join(ryeos_engine::AI_DIR)
        .join("node")
        .join(PROJECTS_DIR)
}

fn owner_key(identity: &ProjectIdentity) -> Result<&str> {
    ryeos_state::refs::principal_storage_key(identity.owner())
}

/// One verified registration and where it was read from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedRegistration {
    pub file: PathBuf,
    pub registration: ProjectRegistration,
}

/// Read and verify every registration under the node key. A missing
/// directory is an empty registry. Links, stray entries, unsigned or
/// foreign-signed files, and records whose owner or name disagree with their
/// path are refused.
pub fn load_registrations(
    app_root: &Path,
    node_key: &lillux::crypto::VerifyingKey,
) -> Result<Vec<LoadedRegistration>> {
    let root = projects_dir(app_root);
    let Some(directory) = lillux::PinnedDirectory::open(&root)
        .with_context(|| format!("open project registrations {}", root.display()))?
    else {
        return Ok(Vec::new());
    };
    let mut loaded = Vec::new();
    for owner_entry in directory.entries_no_follow_bounded(MAX_OWNERS)? {
        let owner_name = owner_entry
            .name
            .to_str()
            .with_context(|| format!("stray entry under {}: name is not UTF-8", root.display()))?;
        ensure!(
            owner_entry.entry_type == lillux::PinnedEntryType::Directory,
            "stray entry {} under {}: only owner directories belong here",
            owner_name,
            root.display()
        );
        let owner = format!("fp:{owner_name}");
        ryeos_state::refs::principal_storage_key(&owner)
            .with_context(|| format!("owner directory {owner_name} under {}", root.display()))?;
        let owner_directory = directory
            .open_child_directory(&owner_entry.name)?
            .with_context(|| format!("owner directory {owner_name} vanished"))?;
        for entry in owner_directory.entries_no_follow_bounded(MAX_NAMES_PER_OWNER)? {
            let file_name = entry.name.to_str().with_context(|| {
                format!(
                    "stray entry under {}: name is not UTF-8",
                    owner_directory.path().display()
                )
            })?;
            let name = file_name
                .strip_suffix(".yaml")
                .filter(|_| entry.entry_type == lillux::PinnedEntryType::Regular);
            let Some(name) = name else {
                bail!(
                    "stray entry {} under {}: only <name>.yaml registration records belong here",
                    file_name,
                    owner_directory.path().display()
                );
            };
            let name = ProjectName::new(name).with_context(|| {
                format!(
                    "registration file name {file_name} under {}",
                    owner_directory.path().display()
                )
            })?;
            let file = owner_directory
                .open_pinned_regular(&entry.name, false)?
                .with_context(|| format!("registration {file_name} vanished"))?;
            let path = file.path().to_path_buf();
            let verified = ryeos_node_runtime::node_document::verify_node_signed_document_file(
                &file, node_key,
            )
            .with_context(|| format!("verify project registration {}", path.display()))?;
            let registration: ProjectRegistration = serde_json::from_value(verified.body)
                .with_context(|| format!("decode project registration {}", path.display()))?;
            registration
                .validate()
                .with_context(|| format!("project registration {}", path.display()))?;
            ensure!(
                registration.identity.owner() == owner && registration.identity.name() == &name,
                "project registration {} names {} but is stored under {owner}/{name}",
                path.display(),
                registration.identity
            );
            loaded.push(LoadedRegistration {
                file: path,
                registration,
            });
        }
    }
    loaded.sort_by(|left, right| left.registration.identity.cmp(&right.registration.identity));
    Ok(loaded)
}

/// What a register or unregister request asks for, after the handler has
/// resolved defaults.
#[derive(Debug, Clone)]
pub struct RegistrationChange<'a> {
    pub app_root: &'a Path,
    pub node: &'a NodeIdentity,
    pub request_digest: &'a str,
    /// Canonical absolute checkout path.
    pub path: &'a Path,
    pub identity: &'a ProjectIdentity,
}

/// The result both operations report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RegistrationOutcome {
    pub identity: ProjectIdentity,
    pub path: PathBuf,
    /// The record's paths after the change; empty when the record is gone.
    pub paths: Vec<PathBuf>,
    pub changed: bool,
}

/// Canonicalize a checkout path, refuse node-internal roots, and read the
/// name the tree declares. Used by register (which requires a declaration)
/// and by doctor (which reports a mismatch).
pub fn inspect_checkout(app_root: &Path, raw: &Path) -> Result<(PathBuf, Option<ProjectName>)> {
    ensure!(
        raw.is_absolute(),
        "checkout path must be absolute: {}",
        raw.display()
    );
    let canonical = std::fs::canonicalize(raw)
        .with_context(|| format!("checkout path {} is not reachable", raw.display()))?;
    validate_checkout_path(&canonical)?;
    ryeos_app::project_root_guard::refuse_node_internal_project_root_for_app(&canonical, app_root)
        .map_err(|refusal| anyhow::anyhow!("{refusal}"))?;
    let pinned = lillux::PinnedDirectory::open(&canonical)?
        .with_context(|| format!("checkout {} is not a directory", canonical.display()))?;
    let declared = ProjectDeclaration::read(&pinned)
        .with_context(|| format!("read the project declaration of {}", canonical.display()))?
        .map(|declaration| declaration.name);
    Ok((canonical, declared))
}

fn read_record(
    owner_directory: &lillux::PinnedDirectory,
    file_name: &OsStr,
    node_key: &lillux::crypto::VerifyingKey,
) -> Result<Option<(lillux::PinnedRegularFile, ProjectRegistration)>> {
    let Some(file) = owner_directory.open_pinned_regular(file_name, false)? else {
        return Ok(None);
    };
    let verified =
        ryeos_node_runtime::node_document::verify_node_signed_document_file(&file, node_key)
            .with_context(|| format!("verify project registration {}", file.path().display()))?;
    let registration: ProjectRegistration = serde_json::from_value(verified.body)
        .with_context(|| format!("decode project registration {}", file.path().display()))?;
    registration.validate()?;
    Ok(Some((file, registration)))
}

fn render(registration: &ProjectRegistration, node: &NodeIdentity) -> Result<Vec<u8>> {
    registration.validate()?;
    ryeos_node_runtime::node_document::render_signed_item(
        PROJECTS_DIR,
        registration.identity.name().as_str(),
        &serde_json::to_value(registration)?,
        node,
    )
}

/// Add `change.path` to the registration of `change.identity`, creating the
/// record when absent. Registering a path already listed rewrites nothing.
pub fn register(change: &RegistrationChange<'_>) -> Result<RegistrationOutcome> {
    validate_checkout_path(change.path)?;
    let projects = lillux::PinnedDirectory::open_or_create(&projects_dir(change.app_root))
        .context("establish the project registrations directory")?;
    let owner_directory = projects
        .open_or_create_child(OsStr::new(owner_key(change.identity)?), 0o700)
        .context("establish the owner's registrations directory")?;
    let _lock = owner_directory.lock_exclusive()?;
    let file_name = format!("{}.yaml", change.identity.name());
    let file_name = OsStr::new(&file_name);
    let existing = read_record(&owner_directory, file_name, change.node.verifying_key())?;
    let mut paths: BTreeSet<PathBuf> = existing
        .as_ref()
        .map(|(_, record)| record.paths.iter().cloned().collect())
        .unwrap_or_default();
    let changed = paths.insert(change.path.to_path_buf());
    let paths: Vec<PathBuf> = paths.into_iter().collect();
    if changed {
        ensure!(
            paths.len() <= MAX_REGISTERED_PATHS,
            "{} already lists {MAX_REGISTERED_PATHS} paths",
            change.identity
        );
        let record = ProjectRegistration {
            schema: PROJECT_REGISTRATION_SCHEMA,
            identity: change.identity.clone(),
            paths: paths.clone(),
            request_digest: change.request_digest.to_owned(),
        };
        let bytes = render(&record, change.node)?;
        owner_directory
            .atomic_write_pinned_if_same(
                file_name,
                existing.as_ref().map(|(file, _)| file),
                &bytes,
                0o600,
                crate::NODE_TEMP_PURPOSE,
            )
            .with_context(|| format!("write the registration of {}", change.identity))?;
    }
    Ok(RegistrationOutcome {
        identity: change.identity.clone(),
        path: change.path.to_path_buf(),
        paths,
        changed,
    })
}

/// Remove `change.path` from the registration of `change.identity`; the
/// record is deleted when no path remains. Unregistering a path that is not
/// listed changes nothing.
pub fn unregister(change: &RegistrationChange<'_>) -> Result<RegistrationOutcome> {
    validate_checkout_path(change.path)?;
    let unchanged = |paths: Vec<PathBuf>| RegistrationOutcome {
        identity: change.identity.clone(),
        path: change.path.to_path_buf(),
        paths,
        changed: false,
    };
    let Some(projects) = lillux::PinnedDirectory::open(&projects_dir(change.app_root))? else {
        return Ok(unchanged(Vec::new()));
    };
    let Some(owner_directory) =
        projects.open_child_directory(OsStr::new(owner_key(change.identity)?))?
    else {
        return Ok(unchanged(Vec::new()));
    };
    let _lock = owner_directory.lock_exclusive()?;
    let file_name = format!("{}.yaml", change.identity.name());
    let file_name = OsStr::new(&file_name);
    let Some((file, record)) =
        read_record(&owner_directory, file_name, change.node.verifying_key())?
    else {
        return Ok(unchanged(Vec::new()));
    };
    let paths: Vec<PathBuf> = record
        .paths
        .iter()
        .filter(|path| path.as_path() != change.path)
        .cloned()
        .collect();
    if paths.len() == record.paths.len() {
        return Ok(unchanged(paths));
    }
    if paths.is_empty() {
        owner_directory
            .remove_pinned_regular_if_same(&file)
            .with_context(|| format!("remove the registration of {}", change.identity))?;
    } else {
        let updated = ProjectRegistration {
            schema: PROJECT_REGISTRATION_SCHEMA,
            identity: change.identity.clone(),
            paths: paths.clone(),
            request_digest: change.request_digest.to_owned(),
        };
        let bytes = render(&updated, change.node)?;
        owner_directory
            .atomic_write_pinned_if_same(
                file_name,
                Some(&file),
                &bytes,
                0o600,
                crate::NODE_TEMP_PURPOSE,
            )
            .with_context(|| format!("write the registration of {}", change.identity))?;
    }
    Ok(RegistrationOutcome {
        identity: change.identity.clone(),
        path: change.path.to_path_buf(),
        paths,
        changed: true,
    })
}
