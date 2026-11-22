//! Checkout registrations: which checkouts (registered live folders on disk)
//! an owner principal has registered under a project name on this node.
//!
//! A registration is a node-signed record
//! `<app_root>/.ai/node/projects/pid-v1-<hex>/<name>.yaml` listing the
//! canonical checkout paths registered for the project
//! `pid:v1:<owner>/<name>`. A pre-cut owner directory (bare key fingerprint
//! hex, schema 1) is never read as trust; it is only reported, so doctor
//! can name it. It is
//! written only by the `node.project.register` / `node.project.unregister`
//! offline operations, which the node's own operator signs on a stopped node.
//!
//! This module is the only reader. A checkout states its project only when
//! the launcher holds live write, the node's ownership resolver says the
//! launcher acts for the registered owner, and the tree still declares that
//! name ([`live_signer_project`]). The writer lives in `ryeos-node` and reads
//! records through [`read_registration`].

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use ryeos_state::project_identity::{ProjectDeclaration, ProjectIdentity, ProjectName};
use serde::{Deserialize, Serialize};

/// Schema of a registration record body. Schema 2 names the owner as a
/// principal id; schema 1 (owner as a key fingerprint) is never read.
pub const PROJECT_REGISTRATION_SCHEMA: u32 = 2;

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
    /// The project, `pid:v1:<64 lowercase hex>/<name>`.
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

pub fn validate_checkout_path(path: &Path) -> Result<()> {
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

/// The owner directory name of a project's registration,
/// `pid-v1-<64 lowercase hex>`.
pub fn owner_directory_name(identity: &ProjectIdentity) -> String {
    identity.owner().storage_component()
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
/// path are refused. Pre-cut owner directories are skipped, never read.
pub fn load_registrations(
    app_root: &Path,
    node_key: &lillux::crypto::VerifyingKey,
) -> Result<Vec<LoadedRegistration>> {
    Ok(load_registrations_reporting(app_root, node_key)?.0)
}

/// [`load_registrations`], plus the pre-cut (schema 1) owner directories
/// it skipped: directories named by a bare key fingerprint hex. Each must be
/// registered again with `ryeos project register`.
pub fn load_registrations_reporting(
    app_root: &Path,
    node_key: &lillux::crypto::VerifyingKey,
) -> Result<(Vec<LoadedRegistration>, Vec<PathBuf>)> {
    let mut pre_cut = Vec::new();
    let root = projects_dir(app_root);
    let Some(directory) = lillux::PinnedDirectory::open(&root)
        .with_context(|| format!("open project registrations {}", root.display()))?
    else {
        return Ok((Vec::new(), pre_cut));
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
        if ryeos_state::refs::principal_storage_key(&format!("fp:{owner_name}")).is_ok() {
            pre_cut.push(root.join(owner_name));
            continue;
        }
        let owner = ryeos_state::principal_id::PrincipalId::from_storage_component(owner_name)
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
            let (file, registration) = read_registration(&owner_directory, &entry.name, node_key)?
                .with_context(|| format!("registration {file_name} vanished"))?;
            let path = file.path().to_path_buf();
            ensure!(
                registration.identity.owner() == &owner && registration.identity.name() == &name,
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
    pre_cut.sort();
    Ok((loaded, pre_cut))
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
    crate::project_root_guard::refuse_node_internal_project_root_for_app(&canonical, app_root)
        .map_err(|refusal| anyhow::anyhow!("{refusal}"))?;
    let pinned = lillux::PinnedDirectory::open(&canonical)?
        .with_context(|| format!("checkout {} is not a directory", canonical.display()))?;
    let declared = ProjectDeclaration::read(&pinned)
        .with_context(|| format!("read the project declaration of {}", canonical.display()))?
        .map(|declaration| declaration.name);
    Ok((canonical, declared))
}

/// Read and verify one registration record under the node key.
pub fn read_registration(
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

/// The project a checkout states for `launcher`: `Stated` only when the
/// launcher holds live project write, the node's ownership resolver (`trust`,
/// the node registry store) says the launcher acts for an owner that
/// registered `checkout`, and the tree still declares that name. Every other
/// case is `Absent(NotRegistered)` (or `NoDeclaredName` when the tree
/// declares none); a retired or revoked launcher key acts for no owner. A
/// registry that cannot be read refuses.
pub fn live_signer_project(
    app_root: &Path,
    node_key: &lillux::crypto::VerifyingKey,
    trust: &ryeos_engine::trust::TrustStore,
    launcher: &str,
    launcher_holds_live_write: bool,
    checkout: &Path,
) -> Result<ryeos_state::project_identity::LogicalProject> {
    use ryeos_state::project_identity::{LogicalProject, LogicalProjectAbsence};
    let not_registered = Ok(LogicalProject::Absent(LogicalProjectAbsence::NotRegistered));
    if !launcher_holds_live_write {
        return not_registered;
    }
    let Ok(owner) = crate::project_owner::owner_for_in(trust, launcher) else {
        return not_registered;
    };
    let (canonical, declared) = inspect_checkout(app_root, checkout)?;
    let Some(declared) = declared else {
        return Ok(LogicalProject::Absent(
            LogicalProjectAbsence::NoDeclaredName,
        ));
    };
    let identity = ProjectIdentity::new(owner.principal, declared);
    let registered = load_registrations(app_root, node_key)?
        .into_iter()
        .any(|loaded| {
            loaded.registration.identity == identity
                && loaded
                    .registration
                    .paths
                    .iter()
                    .any(|path| path == &canonical)
        });
    if registered {
        Ok(LogicalProject::Stated(identity))
    } else {
        not_registered
    }
}

/// [`live_signer_project`] for a caller of this node: the caller is the
/// launcher, and it holds live write only when its scopes authorize
/// `ryeos.write.project.live`. A registry that cannot be read is logged and
/// treated as no registration, which admits no scoped signer.
pub fn live_logical_project(
    state: &crate::state::AppState,
    principal: &str,
    scopes: &[String],
    checkout: &Path,
) -> ryeos_state::project_identity::LogicalProject {
    let holds_live_write = state
        .authorizer
        .authorize(
            scopes,
            &ryeos_runtime::authorizer::AuthorizationPolicy::require(
                ryeos_engine::execution_contract::LIVE_PROJECT_WRITE_CAPABILITY,
            ),
        )
        .is_ok();
    match live_signer_project(
        &state.config.app_root,
        state.identity.verifying_key(),
        &state.engine.trust_store,
        principal,
        holds_live_write,
        checkout,
    ) {
        Ok(project) => project,
        Err(error) => {
            tracing::warn!(
                checkout = %checkout.display(),
                error = %format!("{error:#}"),
                "live project identity unavailable; no scoped signer is admitted"
            );
            ryeos_state::project_identity::LogicalProject::Absent(
                ryeos_state::project_identity::LogicalProjectAbsence::NotRegistered,
            )
        }
    }
}
