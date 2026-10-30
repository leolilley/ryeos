//! Same-root selected-program preparation, never admission or execution.
//!
//! The root host MUST source the realization set from the retained admitted
//! root's PerSpawnDaemon handoff. Typed DTO validation does not authenticate that
//! origin. Namespace-read-only checks do not seal mounts or exclude all writers.
use anyhow::{Context as _, Result, ensure};
use lillux::{PinnedDirectory, PinnedRegularFile};
use ryeos_state::objects::{
    ExternalContentKind, ExternalContentMode, ExternalContentMountRoot, ExternalContentRealization,
    ExternalContentRealizationSet, ExternalRealizationDelivery, MAX_EXTERNAL_CONTENT_FILE_BYTES,
    canonical_value_digest,
};
use std::io::Read as _;
use std::path::{Path, PathBuf};

use crate::finite_product_contract::{
    CheckerMemberSelection, CheckerProgramSelector, FiniteProductContract,
};

/// Where the admitted runtime realizations are reached. The fixed namespace is
/// the read-only mount tree of an enforced launch; a private view is a
/// node-owned directory named by the launch environment, whose entries sit at
/// `<root>/<mount>` and which carries no read-only-namespace guarantee.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RealizationAccess {
    private_root: Option<PathBuf>,
}

impl RealizationAccess {
    /// The enforced-launch default: fixed read-only namespace.
    pub(crate) fn fixed_namespace() -> Self {
        Self { private_root: None }
    }

    /// Read the optional delivery the launch owner selected. Absence means the
    /// fixed namespace; a private delivery requires an absolute normalized root.
    pub(crate) fn from_env() -> Result<Self> {
        let delivery = optional_env("RYEOS_EXTERNAL_DELIVERY")?;
        let root = optional_env("RYEOS_EXTERNAL_ROOT")?;
        Self::from_values(delivery.as_deref(), root.as_deref())
    }

    fn from_values(delivery: Option<&str>, root: Option<&str>) -> Result<Self> {
        let Some(delivery) = delivery else {
            return Ok(Self::fixed_namespace());
        };
        let delivery: ExternalRealizationDelivery =
            serde_json::from_str(delivery).context("decode external realization delivery")?;
        match delivery {
            ExternalRealizationDelivery::FixedNamespace => Ok(Self::fixed_namespace()),
            ExternalRealizationDelivery::PrivateDescriptorRoot => {
                let root = root.context("private realization delivery requires its root")?;
                let path = Path::new(root);
                ensure!(
                    path.is_absolute()
                        && path.components().collect::<PathBuf>() == path
                        && path.as_os_str().as_encoded_bytes()
                            == path
                                .components()
                                .collect::<PathBuf>()
                                .as_os_str()
                                .as_encoded_bytes()
                        && path.components().enumerate().all(|(index, part)| matches!(
                            (index, part),
                            (0, std::path::Component::RootDir)
                                | (1.., std::path::Component::Normal(_))
                        )),
                    "private realization root must be an absolute normalized path"
                );
                Ok(Self {
                    private_root: Some(path.to_path_buf()),
                })
            }
        }
    }

    pub(crate) fn is_private_view(&self) -> bool {
        self.private_root.is_some()
    }

    fn root_path(&self, identity: &ExternalContentRealization) -> Result<PathBuf> {
        match &self.private_root {
            None => identity.mount_root.destination(None, &identity.mount),
            Some(root) => {
                // Validate the logical coordinate exactly as the fixed
                // namespace does, then re-root it under the private view.
                identity.mount_root.destination(None, &identity.mount)?;
                Ok(root.join(&identity.mount))
            }
        }
    }

    /// Path to hand to the checker for an already held pinned location. Under
    /// the fixed namespace the spelling must still select the held inode through
    /// read-only mounts; a private view has no such namespace, so the held
    /// location's own path is used.
    pub(crate) fn directory_path(&self, directory: &PinnedDirectory) -> Result<PathBuf> {
        if self.is_private_view() {
            Ok(directory.path().to_path_buf())
        } else {
            directory.verified_read_only_namespace_path()
        }
    }
}

fn optional_env(name: &str) -> Result<Option<String>> {
    match std::env::var(name) {
        Ok(value) => Ok(Some(value)),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(error) => Err(anyhow::anyhow!("{name} is not valid UTF-8: {error}")),
    }
}

/// Opaque retained data/capabilities selected inside an already admitted root.
/// This owner does not prove applied scope, namespace custody or a child launch.
/// The host retains the selected closure through child/reader/terminal settlement.
pub(crate) struct PreparedCheckerProgram {
    selector: CheckerProgramSelector,
    runtime_identity: ExternalContentRealization,
    code_identity: ExternalContentRealization,
    runtime_root: PinnedDirectory,
    code_root: PinnedDirectory,
    library_directory: PinnedDirectory,
    prefix_directory: PinnedDirectory,
    loader: PinnedRegularFile,
    interpreter: PinnedRegularFile,
    script: PinnedRegularFile,
    script_bytes: Vec<u8>,
    checker_identity_digest: String,
    access: RealizationAccess,
}

impl PreparedCheckerProgram {
    pub(crate) fn checker_identity_digest(&self) -> &str {
        &self.checker_identity_digest
    }
    /// Consume the exact loader into the launch request while retaining every
    /// remaining selected capability. No executable pathname is reopened.
    pub(crate) fn into_launch_parts(self) -> (PinnedRegularFile, CheckerProgramCustody) {
        let Self {
            selector,
            runtime_identity,
            code_identity,
            runtime_root,
            code_root,
            library_directory,
            prefix_directory,
            loader,
            interpreter,
            script,
            script_bytes,
            checker_identity_digest,
            access,
        } = self;
        (
            loader,
            CheckerProgramCustody {
                selector,
                runtime_root,
                library_directory,
                prefix_directory,
                interpreter,
                script_bytes,
                _runtime_identity: runtime_identity,
                _code_identity: code_identity,
                _code_root: code_root,
                _script: script,
                _checker_identity_digest: checker_identity_digest,
                access,
            },
        )
    }
}

/// Root-owned closure retained across child IO and terminal joins. The loader
/// is transferred separately into the pinned Lillux launch request.
pub(crate) struct CheckerProgramCustody {
    pub(crate) selector: CheckerProgramSelector,
    pub(crate) runtime_root: PinnedDirectory,
    pub(crate) library_directory: PinnedDirectory,
    pub(crate) prefix_directory: PinnedDirectory,
    pub(crate) interpreter: PinnedRegularFile,
    pub(crate) script_bytes: Vec<u8>,
    _runtime_identity: ExternalContentRealization,
    _code_identity: ExternalContentRealization,
    _code_root: PinnedDirectory,
    _script: PinnedRegularFile,
    _checker_identity_digest: String,
    pub(crate) access: RealizationAccess,
}

/// Match metadata, never create a purported sealed-set authority wrapper.
fn select_realizations<'a>(
    selector: &CheckerProgramSelector,
    root_set: &'a ExternalContentRealizationSet,
) -> Result<(
    &'a ExternalContentRealization,
    &'a ExternalContentRealization,
)> {
    selector.validate()?;
    root_set.validate()?;
    let select = |id: &str, hash: &str| -> Result<&'a ExternalContentRealization> {
        let selected = root_set
            .iter()
            .find(|entry| entry.id == id)
            .context("selected checker dependency absent from root realization set")?;
        ensure!(
            selected.kind == ExternalContentKind::Tree
                && selected.mode == ExternalContentMode::Pinned
                && selected.mount_root == ExternalContentMountRoot::ExecutionRuntime
                && selected.manifest_hash == hash,
            "selected checker dependency kind/mode/root/manifest mismatch"
        );
        // Set validation already guarantees unique IDs/nonoverlapping mounts;
        // destination uses the SAME canonical logical mount selected here.
        selected.mount_root.destination(None, &selected.mount)?;
        Ok(selected)
    };
    let runtime = select(&selector.runtime.id, &selector.runtime.manifest_hash)?;
    let code = select(&selector.code.id, &selector.code.manifest_hash)?;
    let runtime_bytes = selector
        .runtime
        .loader
        .bytes
        .checked_add(selector.runtime.interpreter.bytes)
        .context("checker runtime member byte aggregate overflow")?;
    ensure!(
        runtime.total_bytes >= runtime_bytes && code.total_bytes >= selector.code.script.bytes,
        "selected members exceed their retained realization byte totals"
    );
    Ok((runtime, code))
}

fn open_root(
    identity: &ExternalContentRealization,
    access: &RealizationAccess,
) -> Result<PinnedDirectory> {
    let destination = access.root_path(identity)?;
    let root =
        PinnedDirectory::open(&destination)?.context("selected checker tree mount absent")?;
    if !access.is_private_view() {
        ensure!(
            root.verified_read_only_namespace_path()? == destination,
            "checker root read-only namespace observation changed"
        );
    }
    Ok(root)
}

fn open_directory(
    root: &PinnedDirectory,
    relative: &str,
    access: &RealizationAccess,
) -> Result<PinnedDirectory> {
    ryeos_state::objects::validate_canonical_project_relative_path(relative)?;
    let mut current = root.try_clone()?;
    for component in Path::new(relative).components() {
        let std::path::Component::Normal(name) = component else {
            anyhow::bail!("checker directory path is not canonical");
        };
        current = current
            .open_child_directory(name)?
            .context("checker runtime directory absent")?;
    }
    let selected = root.path().join(relative);
    ensure!(
        access.directory_path(&current)? == selected,
        "checker runtime directory escaped read-only namespace observation"
    );
    Ok(current)
}

fn open_member(
    root: &PinnedDirectory,
    selected: &CheckerMemberSelection,
    access: &RealizationAccess,
) -> Result<PinnedRegularFile> {
    ryeos_state::objects::validate_canonical_project_relative_path(&selected.path)?;
    let member = root
        .open_pinned_regular_descendant(Path::new(&selected.path), false)?
        .context("selected checker member absent")?;
    if !access.is_private_view() {
        ensure!(
            member.verified_read_only_namespace_path()? == root.path().join(&selected.path),
            "checker member escaped read-only namespace observation"
        );
    }
    Ok(member)
}

fn verify_executable(member: &PinnedRegularFile, selected: &CheckerMemberSelection) -> Result<()> {
    ensure!(
        (1..=MAX_EXTERNAL_CONTENT_FILE_BYTES).contains(&selected.bytes),
        "checker executable exceeds cap"
    );
    member.require_executable()?;
    let (actual, _) = lillux::secure_fs::digest_open_regular_file_stable_exact(
        &member.try_clone_descriptor()?,
        selected.bytes,
    )?;
    ensure!(
        actual == selected.sha256,
        "checker executable descriptor digest mismatch"
    );
    Ok(())
}

fn verify_script_snapshot(bytes: &[u8], selected: &CheckerMemberSelection) -> Result<()> {
    ensure!(
        (1..=MAX_EXTERNAL_CONTENT_FILE_BYTES).contains(&selected.bytes)
            && u64::try_from(bytes.len())? == selected.bytes
            && lillux::sha256_hex(bytes) == selected.sha256,
        "retained checker script snapshot differs from selected exact bytes/hash"
    );
    Ok(())
}

fn read_script(member: &PinnedRegularFile, selected: &CheckerMemberSelection) -> Result<Vec<u8>> {
    // One stable descriptor read binds the actual retained Vec; no earlier
    // digest followed by an unrelated/unconstrained reread is accepted.
    let mut reader = member.stable_reader_exact(
        selected.bytes,
        &selected.sha256,
        MAX_EXTERNAL_CONTENT_FILE_BYTES,
    )?;
    let mut bytes = Vec::with_capacity(usize::try_from(selected.bytes)?);
    reader.read_to_end(&mut bytes)?;
    reader.finish()?;
    verify_script_snapshot(&bytes, selected)?;
    Ok(bytes)
}

fn program_identity(
    selector: &CheckerProgramSelector,
    runtime: &ExternalContentRealization,
    code: &ExternalContentRealization,
) -> Result<String> {
    canonical_value_digest(&serde_json::json!({
        "domain":"ryeos.codex.selected-checker-program.v1",
        "selector":selector, "runtime_realization":runtime, "code_realization":code,
    }))
}

/// Preparation only. Contract/set provenance and immutable namespace
/// custody MUST come from the admitted root caller; DTO shape is not
/// authentication. Missing actual dependencies refuses; no ambient fallback.
pub(crate) fn prepare_checker_program(
    contract: &FiniteProductContract,
    root_realizations: &ExternalContentRealizationSet,
    access: &RealizationAccess,
) -> Result<PreparedCheckerProgram> {
    contract.validate()?;
    let selector = contract.semantic_checker();
    let (runtime, code) = select_realizations(selector, root_realizations)?;
    let runtime_root = open_root(runtime, access)?;
    let code_root = open_root(code, access)?;
    let library_directory = open_directory(&runtime_root, &selector.runtime.library_path, access)?;
    let prefix_directory = open_directory(&runtime_root, &selector.runtime.prefix_path, access)?;
    let loader = open_member(&runtime_root, &selector.runtime.loader, access)?;
    let interpreter = open_member(&runtime_root, &selector.runtime.interpreter, access)?;
    let script = open_member(&code_root, &selector.code.script, access)?;
    verify_executable(&loader, &selector.runtime.loader)?;
    verify_executable(&interpreter, &selector.runtime.interpreter)?;
    let script_bytes = read_script(&script, &selector.code.script)?;
    let checker_identity_digest = program_identity(selector, runtime, code)?;
    Ok(PreparedCheckerProgram {
        selector: selector.clone(),
        runtime_identity: runtime.clone(),
        code_identity: code.clone(),
        runtime_root,
        code_root,
        library_directory,
        prefix_directory,
        loader,
        interpreter,
        script,
        script_bytes,
        checker_identity_digest,
        access: access.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // Synthetic metadata is deliberately unauthenticated: these tests cannot
    // grant runtime admission or bypass actual pinned readonly member checks.
    fn set(selector: &CheckerProgramSelector) -> ExternalContentRealizationSet {
        ExternalContentRealizationSet::new(vec![
            ExternalContentRealization {
                id: selector.runtime.id.clone(),
                kind: ExternalContentKind::Tree,
                mode: ExternalContentMode::Pinned,
                manifest_hash: selector.runtime.manifest_hash.clone(),
                entry_count: 2,
                total_bytes: selector.runtime.loader.bytes + selector.runtime.interpreter.bytes,
                mount_root: ExternalContentMountRoot::ExecutionRuntime,
                mount: "test-checker-runtime".to_owned(),
            },
            ExternalContentRealization {
                id: selector.code.id.clone(),
                kind: ExternalContentKind::Tree,
                mode: ExternalContentMode::Pinned,
                manifest_hash: selector.code.manifest_hash.clone(),
                entry_count: 1,
                total_bytes: selector.code.script.bytes,
                mount_root: ExternalContentMountRoot::ExecutionRuntime,
                mount: "test-checker-code".to_owned(),
            },
        ])
        .unwrap()
    }

    #[test]
    fn private_delivery_roots_realizations_under_the_node_view() {
        let selector = crate::finite_product_contract::checker_selector_fixture();
        let root = set(&selector);
        let (runtime, _) = select_realizations(&selector, &root).unwrap();
        let private =
            serde_json::to_string(&ExternalRealizationDelivery::PrivateDescriptorRoot).unwrap();
        let fixed = serde_json::to_string(&ExternalRealizationDelivery::FixedNamespace).unwrap();

        let access = RealizationAccess::from_values(Some(&private), Some("/node/view")).unwrap();
        assert!(access.is_private_view());
        assert_eq!(
            access.root_path(runtime).unwrap(),
            Path::new("/node/view/test-checker-runtime")
        );

        for absent in [
            RealizationAccess::from_values(None, None).unwrap(),
            RealizationAccess::from_values(Some(&fixed), Some("/node/view")).unwrap(),
        ] {
            assert!(!absent.is_private_view());
            assert_eq!(
                absent.root_path(runtime).unwrap(),
                Path::new("/ryeos/realizations/test-checker-runtime")
            );
        }

        assert!(RealizationAccess::from_values(Some(&private), None).is_err());
        for bad in ["relative/view", "/node/../view", "/node/./view"] {
            assert!(RealizationAccess::from_values(Some(&private), Some(bad)).is_err());
        }
        assert!(RealizationAccess::from_values(Some("\"other\""), Some("/node/view")).is_err());
    }

    #[test]
    fn dependency_match_uses_complete_same_root_identity_and_refuses_absence() {
        let selector = crate::finite_product_contract::checker_selector_fixture();
        let root = set(&selector);
        let (runtime, code) = select_realizations(&selector, &root).unwrap();
        assert_eq!(runtime.id, selector.runtime.id);
        assert_eq!(code.id, selector.code.id);
        assert!(select_realizations(&selector, &ExternalContentRealizationSet::default()).is_err());
        for mutation in ["hash", "kind", "mode", "root", "size"] {
            let mut entries = root.iter().cloned().collect::<Vec<_>>();
            let runtime = entries
                .iter_mut()
                .find(|e| e.id == selector.runtime.id)
                .unwrap();
            match mutation {
                "hash" => runtime.manifest_hash = "0".repeat(64),
                "kind" => runtime.kind = ExternalContentKind::File,
                "mode" => runtime.mode = ExternalContentMode::Captured,
                "root" => runtime.mount_root = ExternalContentMountRoot::Project,
                _ => runtime.total_bytes = 1,
            }
            let changed = ExternalContentRealizationSet::new(entries).unwrap();
            assert!(
                select_realizations(&selector, &changed).is_err(),
                "{mutation}"
            );
        }
    }

    #[test]
    fn shipped_runtime_and_code_ids_select_and_refuse_wrong_entries() {
        let selector = crate::finite_product_contract::checker_selector_fixture();
        assert_eq!(selector.runtime.id, "checker-runtime");
        assert_eq!(selector.code.id, "checker-code");
        let root = set(&selector);
        let (runtime, code) = select_realizations(&selector, &root).unwrap();
        assert_eq!(runtime.manifest_hash, selector.runtime.manifest_hash);
        assert_eq!(code.manifest_hash, selector.code.manifest_hash);

        let mut entries = root.iter().cloned().collect::<Vec<_>>();
        entries
            .iter_mut()
            .find(|e| e.id == "checker-code")
            .unwrap()
            .manifest_hash = "0".repeat(64);
        let changed = ExternalContentRealizationSet::new(entries).unwrap();
        assert!(select_realizations(&selector, &changed).is_err());

        let mut entries = root.iter().cloned().collect::<Vec<_>>();
        entries
            .iter_mut()
            .find(|e| e.id == "checker-runtime")
            .unwrap()
            .mount_root = ExternalContentMountRoot::Project;
        let changed = ExternalContentRealizationSet::new(entries).unwrap();
        assert!(select_realizations(&selector, &changed).is_err());

        let mut entries = root.iter().cloned().collect::<Vec<_>>();
        for entry in &mut entries {
            entry.id = if entry.id == "checker-runtime" {
                "checker-code".to_owned()
            } else {
                "checker-runtime".to_owned()
            };
        }
        let swapped = ExternalContentRealizationSet::new(entries).unwrap();
        assert!(select_realizations(&selector, &swapped).is_err());
    }

    #[test]
    fn retained_script_bytes_not_a_prior_digest_determine_snapshot_acceptance() {
        let mut selected = crate::finite_product_contract::checker_selector_fixture()
            .code
            .script;
        selected.bytes = 3;
        selected.sha256 = lillux::sha256_hex(b"abc");
        assert!(verify_script_snapshot(b"abc", &selected).is_ok());
        assert!(verify_script_snapshot(b"abd", &selected).is_err());
        assert!(verify_script_snapshot(b"abcd", &selected).is_err());
        selected.bytes = MAX_EXTERNAL_CONTENT_FILE_BYTES + 1;
        assert!(verify_script_snapshot(b"abc", &selected).is_err());
    }

    #[test]
    fn selected_program_identity_commits_selector_and_exact_mount_inventory() {
        let selector = crate::finite_product_contract::checker_selector_fixture();
        let root = set(&selector);
        let (runtime, code) = select_realizations(&selector, &root).unwrap();
        let identity = program_identity(&selector, runtime, code).unwrap();
        let mut changed = selector.clone();
        changed.runtime.prefix_path = "different-prefix".to_owned();
        assert_ne!(identity, program_identity(&changed, runtime, code).unwrap());
        let mut code = code.clone();
        code.mount = "different-code-mount".to_owned();
        assert_ne!(
            identity,
            program_identity(&selector, runtime, &code).unwrap()
        );
    }

    #[cfg(unix)]
    #[test]
    fn exact_member_descriptor_refuses_nonexecutability_size_and_hash_changes() {
        use std::os::unix::fs::PermissionsExt as _;
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("member");
        std::fs::write(&path, b"opaque test bytes; never executed").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let directory = PinnedDirectory::open(temporary.path()).unwrap().unwrap();
        let member = directory
            .open_pinned_regular(std::ffi::OsStr::new("member"), false)
            .unwrap()
            .unwrap();
        let mut selected = CheckerMemberSelection {
            path: "member".to_owned(),
            bytes: b"opaque test bytes; never executed".len() as u64,
            sha256: lillux::sha256_hex(b"opaque test bytes; never executed"),
        };
        assert!(verify_executable(&member, &selected).is_err());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(verify_executable(&member, &selected).is_ok());
        selected.sha256 = "0".repeat(64);
        assert!(verify_executable(&member, &selected).is_err());
        selected.bytes += 1;
        assert!(verify_executable(&member, &selected).is_err());
        // This lower-level descriptor control is NOT readonly root admission,
        // an ELF check, a process launch, or a qualification claim.
    }
}
