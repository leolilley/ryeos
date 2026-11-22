//! Writing live project registrations: the `node.project.register` and
//! `node.project.unregister` offline operations' effect. The reader, and the
//! only place records are verified, is `ryeos_app::project_registry`.

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use ryeos_node_runtime::identity::NodeIdentity;
use ryeos_state::project_identity::ProjectIdentity;
use serde::Serialize;

pub use ryeos_app::project_registry::{
    LoadedRegistration, MAX_REGISTERED_PATHS, PROJECT_REGISTRATION_SCHEMA, PROJECTS_DIR,
    ProjectRegistration, inspect_checkout, live_signer_project, load_registrations, owner_key,
    projects_dir, read_registration,
};

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
    let existing = read_registration(&owner_directory, file_name, change.node.verifying_key())?;
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
        read_registration(&owner_directory, file_name, change.node.verifying_key())?
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
