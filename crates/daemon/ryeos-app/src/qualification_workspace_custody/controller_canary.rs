//! Private original-descriptor custody, not a qualification verdict. The caller
//! must retain a durable creation intent before create(), bind the complete
//! original guest input selection/leases, and hold its existing occurrence gate
//! through recheck and journal commit. Serialized observations cannot reopen this.
use anyhow::{Context as _, Result, ensure};
use lillux::{
    InheritedDescriptorAuthority, OpenRegularFileObservation, PinnedDirectory, PinnedRegularFile,
};
use ryeos_state::external_execution::qualification_controller_canary::{
    QControllerCanaryFileObservation, validate_controller_path,
};
use std::ffi::OsStr;
use zeroize::Zeroizing;

const DIRECTORY: &str = "controller-canary";
const FILE: &str = "canary";
const BYTES: u64 = 65;
// Base + optional outputs + 64 mounts, each with at most two source records.
const MAX_SOURCES: usize = 2 + 3 * ryeos_external_execution_contract::MAX_GUEST_INPUTS;

#[derive(Default)]
pub(super) enum CanarySlot {
    #[default]
    Uncreated,
    Attempted,
    Original(OriginalCanary),
}

impl CanarySlot {
    pub(super) fn create(
        &mut self,
        state: &PinnedDirectory,
        workspace: &PinnedDirectory,
        credential_home: &PinnedDirectory,
        original_guest_sources: &[InheritedDescriptorAuthority],
    ) -> Result<(String, QControllerCanaryFileObservation)> {
        ensure!(
            matches!(self, Self::Uncreated),
            "Q canary creation already attempted"
        );
        // A failure cannot turn partial filesystem effects into a retry permit.
        *self = Self::Attempted;
        let original =
            OriginalCanary::create(state, workspace, credential_home, original_guest_sources)?;
        let observed = original.recheck(state)?;
        *self = Self::Original(original);
        Ok(observed)
    }

    pub(super) fn recheck(
        &self,
        state: &PinnedDirectory,
    ) -> Result<(String, QControllerCanaryFileObservation)> {
        let Self::Original(original) = self else {
            anyhow::bail!("Q canary has no original live observation in this daemon");
        };
        original.recheck(state)
    }
}

// Intentionally neither serializable nor Debug: original observations and
// descriptors survive only in the original daemon's existing Q custody entry.
pub(super) struct OriginalCanary {
    directory: PinnedDirectory,
    file: PinnedRegularFile,
    observed: OpenRegularFileObservation,
    public: QControllerCanaryFileObservation,
    path: String,
    workspace: PinnedDirectory,
    credential_home: PinnedDirectory,
    guest_sources: Vec<InheritedDescriptorAuthority>,
}

impl OriginalCanary {
    fn create(
        state: &PinnedDirectory,
        workspace: &PinnedDirectory,
        credential_home: &PinnedDirectory,
        sources: &[InheritedDescriptorAuthority],
    ) -> Result<Self> {
        ensure!(
            !sources.is_empty() && sources.len() <= MAX_SOURCES,
            "Q canary guest descriptor inventory is outside its bound"
        );
        state.require_owner_private_directory()?;
        state.ensure_path_binding()?;
        workspace.ensure_path_binding()?;
        credential_home.require_owner_private_directory()?;
        credential_home.ensure_path_binding()?;
        state.require_disjoint_directory_tree(workspace)?;
        state.require_disjoint_directory_tree(credential_home)?;
        let path = state
            .path()
            .join(DIRECTORY)
            .join(FILE)
            .to_str()
            .context("Q canary path is not UTF-8")?
            .to_owned();
        validate_controller_path(&path)?;
        let directory = state.create_child(OsStr::new(DIRECTORY), 0o700)?;
        require_guest_directories_disjoint(&directory, sources)?;
        let mut entropy = Zeroizing::new([0_u8; 32]);
        lillux::crypto::fill_random_bytes(&mut entropy[..])?;
        let value = Zeroizing::new(lillux::sha256_hex(&entropy[..]));
        let bytes = Zeroizing::new(format!("{}\n", value.as_str()));
        let file = directory
            .atomic_create_pinned_regular(OsStr::new(FILE), bytes.as_bytes(), 0o600)?
            .context("Q original canary leaf already exists")?;
        require_private_single_link(&file)?;
        let observed = file.observation()?;
        let file_identity = {
            let _lease = lillux::exec::retain_fork_sensitive_descriptors();
            lillux::pinned_regular_file_identity(&file.try_clone_descriptor()?)?
        };
        let public = QControllerCanaryFileObservation {
            directory_identity: directory.identity()?,
            file_identity,
            content_sha256: lillux::sha256_hex(bytes.as_bytes()),
            size_bytes: observed.size(),
            permission_mode: observed.full_permission_mode()?,
        };
        public.validate()?;
        let original = Self {
            directory,
            file,
            observed,
            public,
            path,
            workspace: workspace.try_clone()?,
            credential_home: credential_home.try_clone()?,
            guest_sources: sources.to_vec(),
        };
        original.require_sources_disjoint()?;
        Ok(original)
    }

    fn require_sources_disjoint(&self) -> Result<()> {
        self.workspace.ensure_path_binding()?;
        self.credential_home.require_owner_private_directory()?;
        self.credential_home.ensure_path_binding()?;
        self.directory
            .require_disjoint_directory_tree(&self.workspace)?;
        self.directory
            .require_disjoint_directory_tree(&self.credential_home)?;
        require_guest_directories_disjoint(&self.directory, &self.guest_sources)?;
        let canary = self.file.inherited_descriptor_authority()?;
        for source in &self.guest_sources {
            ensure!(
                !canary.same_file_identity(source)?,
                "Q canary inode is an original guest input"
            );
        }
        Ok(())
    }

    fn recheck(
        &self,
        state: &PinnedDirectory,
    ) -> Result<(String, QControllerCanaryFileObservation)> {
        state.require_owner_private_directory()?;
        state.ensure_path_binding()?;
        let current_directory = state
            .open_child_directory(OsStr::new(DIRECTORY))?
            .context("Q original canary directory is missing")?;
        ensure!(
            current_directory.is_same_directory(&self.directory)?,
            "Q original canary directory was replaced"
        );
        self.directory.require_owner_private_directory()?;
        self.directory.ensure_path_binding()?;
        self.require_sources_disjoint()?;
        let current = self
            .directory
            .open_pinned_regular(OsStr::new(FILE), false)?
            .context("Q original canary file is missing")?;
        require_private_single_link(&current)?;
        require_private_single_link(&self.file)?;
        let bytes = Zeroizing::new(current.read_stable_bounded(&self.observed, BYTES)?);
        ensure!(
            bytes.len() == BYTES as usize
                && lillux::sha256_hex(&bytes) == self.public.content_sha256,
            "Q original canary content changed"
        );
        // Original inode and namespace-selected inode must both remain stable.
        let held = Zeroizing::new(self.file.read_stable_bounded(&self.observed, BYTES)?);
        ensure!(
            held.as_slice() == bytes.as_slice(),
            "Q canary held inode changed"
        );
        self.directory.ensure_path_binding()?;
        state.ensure_path_binding()?;
        Ok((self.path.clone(), self.public.clone()))
    }
}

fn require_private_single_link(file: &PinnedRegularFile) -> Result<()> {
    let _lease = lillux::exec::retain_fork_sensitive_descriptors();
    let descriptor = file.try_clone_descriptor()?;
    let identity = lillux::require_effective_user_owned_regular(&descriptor)?;
    file.require_owner(identity.owner())?;
    ensure!(
        file.observation()?.full_permission_mode()? == 0o600,
        "Q canary permission mode changed"
    );
    Ok(())
}

fn require_guest_directories_disjoint(
    directory: &PinnedDirectory,
    sources: &[InheritedDescriptorAuthority],
) -> Result<()> {
    for source in sources {
        match source.mount_entry_kind()? {
            lillux::OpenMountEntryKind::Directory => {
                let pinned = source.try_clone_pinned_directory(source.path().to_path_buf())?;
                directory.require_disjoint_directory_tree(&pinned)?;
            }
            lillux::OpenMountEntryKind::Regular => {}
            _ => anyhow::bail!("Q canary exclusion inventory has an unsupported descriptor kind"),
        }
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    struct Fixture {
        _tmp: tempfile::TempDir,
        root: PinnedDirectory,
        state: PinnedDirectory,
        workspace: PinnedDirectory,
        home: PinnedDirectory,
        sources: Vec<InheritedDescriptorAuthority>,
    }
    impl Fixture {
        fn new() -> Self {
            let tmp = tempfile::tempdir().unwrap();
            let root = PinnedDirectory::open(tmp.path()).unwrap().unwrap();
            let state = root.create_child(OsStr::new("state"), 0o700).unwrap();
            let workspace = root.create_child(OsStr::new("project"), 0o700).unwrap();
            let home = root.create_child(OsStr::new("home"), 0o700).unwrap();
            let guest = root.create_child(OsStr::new("guest"), 0o700).unwrap();
            let sources = vec![guest.inherited_descriptor_authority().unwrap()];
            Self {
                _tmp: tmp,
                root,
                state,
                workspace,
                home,
                sources,
            }
        }
        fn create(&self, slot: &mut CanarySlot) -> (String, QControllerCanaryFileObservation) {
            slot.create(&self.state, &self.workspace, &self.home, &self.sources)
                .unwrap()
        }
    }

    #[test]
    fn canary_is_fresh_private_stable_and_has_no_recreation_lane() {
        let fixture = Fixture::new();
        let mut slot = CanarySlot::default();
        assert!(slot.recheck(&fixture.state).is_err());
        let first = fixture.create(&mut slot);
        assert_eq!(first.1.size_bytes, 65);
        assert_eq!(first.1.permission_mode, 0o600);
        let bytes = std::fs::read(&first.0).unwrap();
        assert_eq!(bytes.len(), 65);
        assert_eq!(bytes[64], b'\n');
        assert!(lillux::valid_hash(
            std::str::from_utf8(&bytes[..64]).unwrap()
        ));
        assert_eq!(slot.recheck(&fixture.state).unwrap(), first);
        assert!(
            slot.create(
                &fixture.state,
                &fixture.workspace,
                &fixture.home,
                &fixture.sources
            )
            .is_err()
        );
        let mut cold = CanarySlot::default();
        assert!(cold.recheck(&fixture.state).is_err());
        assert!(
            cold.create(
                &fixture.state,
                &fixture.workspace,
                &fixture.home,
                &fixture.sources
            )
            .is_err()
        );
        assert!(cold.recheck(&fixture.state).is_err());
        assert_eq!(std::fs::read(&first.0).unwrap(), bytes);
    }

    #[test]
    fn canary_refuses_changed_content_missing_file_and_same_bytes_new_inode() {
        for variant in 0..5 {
            let fixture = Fixture::new();
            let mut slot = CanarySlot::default();
            let (path, _) = fixture.create(&mut slot);
            match variant {
                0 => std::fs::write(&path, vec![b'x'; 65]).unwrap(),
                1 => std::fs::remove_file(&path).unwrap(),
                2 => {
                    let bytes = std::fs::read(&path).unwrap();
                    std::fs::rename(&path, format!("{path}-original")).unwrap();
                    std::fs::write(&path, bytes).unwrap();
                    use std::os::unix::fs::PermissionsExt as _;
                    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
                        .unwrap();
                }
                3 => std::fs::hard_link(&path, format!("{path}-alias")).unwrap(),
                _ => {
                    std::fs::rename(&path, format!("{path}-original")).unwrap();
                    std::os::unix::fs::symlink(format!("{path}-original"), &path).unwrap();
                }
            }
            assert!(slot.recheck(&fixture.state).is_err());
        }
    }

    #[test]
    fn canary_refuses_parent_replacement_and_directory_overlap_but_allows_siblings() {
        let fixture = Fixture::new();
        let mut slot = CanarySlot::default();
        fixture.create(&mut slot);
        let path = fixture.state.path().join(DIRECTORY);
        std::fs::rename(&path, fixture.state.path().join("old-canary")).unwrap();
        fixture
            .state
            .create_child(OsStr::new(DIRECTORY), 0o700)
            .unwrap();
        assert!(slot.recheck(&fixture.state).is_err());
        for source_is_parent in [false, true] {
            let fixture = Fixture::new();
            let source = if source_is_parent {
                &fixture.root
            } else {
                &fixture.state
            };
            let sources = vec![source.inherited_descriptor_authority().unwrap()];
            let mut slot = CanarySlot::default();
            assert!(
                slot.create(&fixture.state, &fixture.workspace, &fixture.home, &sources)
                    .is_err()
            );
            // Failure consumes this original attempt even if the empty child is removed.
            let child = fixture
                .state
                .open_child_directory(OsStr::new(DIRECTORY))
                .unwrap()
                .unwrap();
            fixture
                .state
                .remove_empty_child_if_same(OsStr::new(DIRECTORY), &child)
                .unwrap();
            assert!(
                slot.create(
                    &fixture.state,
                    &fixture.workspace,
                    &fixture.home,
                    &fixture.sources
                )
                .is_err()
            );
        }
        let fixture = Fixture::new();
        let guest_sibling = fixture
            .state
            .create_child(OsStr::new("guest-sibling"), 0o700)
            .unwrap();
        let sources = vec![guest_sibling.inherited_descriptor_authority().unwrap()];
        let mut slot = CanarySlot::default();
        slot.create(&fixture.state, &fixture.workspace, &fixture.home, &sources)
            .unwrap();
        assert!(slot.recheck(&fixture.state).is_ok());
    }
}
