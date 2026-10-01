//! Native custody of one separately admitted Q fixture.
//!
//! This helper creates no mount or CAS authority. The native launcher must
//! associate its sole fixture descriptor with its held/applied mount receipt,
//! then supply that same process's opaque namespace-termination proof. Capture
//! facts are not a signed settlement or a qualification claim.

use std::ffi::OsStr;

use anyhow::{Context as _, Result, ensure};
use lillux::time::MonotonicDeadline;
use ryeos_external_execution_contract::qualification_fixture::QFixtureContract;
use ryeos_state::objects::ProjectFile;

/// Original launcher-owned backing directory, never reopened from a caller's
/// pathname. Its parent owner retains cleanup custody, including partial create
/// or failed capture; dropping this value does not delete a potentially live
/// mounted directory. Do not clone its backing authority into controller state.
pub(crate) struct OwnedQualificationFixture {
    directory: lillux::PinnedDirectory,
    contract: QFixtureContract,
    capture_attempted: bool,
}

/// Bounded observed content only. The guest journal must independently join
/// native/runtime/completion identities before constructing durable metadata.
pub(crate) struct CapturedQualificationFixture {
    pub(crate) bytes: Vec<u8>,
    pub(crate) blob_hash: String,
    pub(crate) normalized_mode: u32,
}

impl OwnedQualificationFixture {
    pub(crate) fn create(
        private_parent: &lillux::PinnedDirectory,
        contract: &QFixtureContract,
        deadline: MonotonicDeadline,
    ) -> Result<Self> {
        require_time(deadline)?;
        let bytes = contract.decoded_initial_content()?;
        private_parent.require_owner_private_directory()?;
        require_time(deadline)?;
        let (_, directory) = private_parent.create_unique_child("qualification-fixture", 0o700)?;
        require_time(deadline)?;
        directory
            .atomic_create_regular(
                OsStr::new(&contract.leaf),
                &bytes,
                ProjectFile::REGULAR_MODE,
            )?
            .context("new Q fixture leaf already exists")?;
        require_time(deadline)?;
        Ok(Self {
            directory,
            contract: contract.clone(),
            capture_attempted: false,
        })
    }

    /// Only the dedicated native launcher may use this descriptor to build
    /// the fixed signed fixture mount and retain its applied identity. The
    /// descriptor must not become an independently retained writable alias.
    pub(crate) fn mount_authority(&self) -> Result<lillux::InheritedDescriptorAuthority> {
        ensure!(
            !self.capture_attempted,
            "Q fixture capture already attempted"
        );
        self.directory.require_owner_private_directory()?;
        self.directory.inherited_descriptor_authority()
    }

    /// The caller must pass its own retained proof for the exact namespace
    /// mounted from this fixture. Lillux alone constructs this opaque proof;
    /// neither a wire exit code nor caller-authored settlement data suffices.
    /// An error consumes the capture attempt: recovery needs retained immutable
    /// evidence, never a second read of a possibly changed directory.
    pub(crate) fn capture_after_namespace_reap(
        &mut self,
        proof: &lillux::LinuxSandboxTermination,
        deadline: MonotonicDeadline,
    ) -> Result<CapturedQualificationFixture> {
        ensure!(
            !self.capture_attempted,
            "Q fixture capture already attempted"
        );
        self.capture_attempted = true;
        ensure!(
            proof.launch_failure().is_none(),
            "Q fixture workload failed before native execution"
        );
        self.read_frozen_file(deadline)
    }

    // Filesystem validation alone is deliberately private. Tests exercise
    // these checks without fabricating a native settlement proof.
    fn read_frozen_file(
        &self,
        deadline: MonotonicDeadline,
    ) -> Result<CapturedQualificationFixture> {
        #[cfg(not(unix))]
        {
            let _ = deadline;
            anyhow::bail!("Q fixture capture requires native descriptor custody")
        }
        #[cfg(unix)]
        {
            require_time(deadline)?;
            self.directory.require_owner_private_directory()?;
            // This kernel walk checks owner, mount crossings and single-link
            // regular inodes. Its opaque-symlink allowance is narrowed below.
            self.directory.require_owner_enclosed_tree_bounded(
                lillux::DirectoryTraversalBudget::new(1, 1),
                self.contract.max_bytes,
            )?;
            let entries = self.directory.entries_no_follow_bounded(1)?;
            ensure!(
                entries.len() == 1
                    && entries[0].name.as_os_str() == OsStr::new(&self.contract.leaf)
                    && entries[0].entry_type == lillux::PinnedEntryType::Regular,
                "Q fixture must contain exactly its one regular leaf"
            );
            let entry = &entries[0];
            self.directory.ensure_entry_same_mount(entry)?;
            let file = self
                .directory
                .open_pinned_regular(&entry.name, false)?
                .context("Q fixture leaf disappeared")?;
            let observation = file.observation()?;
            ensure!(
                observation.matches_directory_entry(entry)
                    && observation.full_permission_mode()? & 0o7111 == 0,
                "Q fixture changed its regular inode or acquired executable/special mode"
            );
            require_time(deadline)?;
            let bytes = file.read_stable_bounded(&observation, self.contract.max_bytes)?;
            // Recheck the complete namespace and hard-link/owner boundary
            // after reading, rather than authenticating only the named file.
            self.directory.require_owner_enclosed_tree_bounded(
                lillux::DirectoryTraversalBudget::new(1, 1),
                self.contract.max_bytes,
            )?;
            ensure!(
                self.directory.entries_no_follow_bounded(1)? == entries,
                "Q fixture namespace changed during capture"
            );
            self.directory.ensure_entry_observation(entry)?;
            require_time(deadline)?;
            Ok(CapturedQualificationFixture {
                blob_hash: lillux::sha256_hex(&bytes),
                bytes,
                normalized_mode: ProjectFile::REGULAR_MODE,
            })
        }
    }
}

fn require_time(deadline: MonotonicDeadline) -> Result<()> {
    ensure!(!deadline.has_elapsed(), "Q fixture deadline expired");
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use ryeos_external_execution_contract::qualification_fixture::{
        MAX_Q_FIXTURE_BYTES, Q_FIXTURE_CONTRACT_SCHEMA, Q_FIXTURE_MOUNT_DESTINATION,
    };

    fn deadline() -> MonotonicDeadline {
        MonotonicDeadline::after(lillux::time::Duration::from_secs(30))
    }

    fn make_fixture(bytes: &[u8]) -> (tempfile::TempDir, OwnedQualificationFixture) {
        let temporary = tempfile::tempdir().unwrap();
        let parent = lillux::PinnedDirectory::open(temporary.path())
            .unwrap()
            .unwrap();
        parent.set_mode(0o700).unwrap();
        let contract = QFixtureContract {
            schema: Q_FIXTURE_CONTRACT_SCHEMA,
            mount_destination: Q_FIXTURE_MOUNT_DESTINATION.into(),
            leaf: "candidate.txt".into(),
            initial_content_b64: STANDARD.encode(bytes),
            initial_blob_hash: lillux::sha256_hex(bytes),
            max_bytes: MAX_Q_FIXTURE_BYTES,
        };
        let fixture = OwnedQualificationFixture::create(&parent, &contract, deadline()).unwrap();
        (temporary, fixture)
    }

    fn remove_leaf(fixture: &OwnedQualificationFixture) {
        let file = fixture
            .directory
            .open_pinned_regular(OsStr::new(&fixture.contract.leaf), false)
            .unwrap()
            .unwrap();
        fixture
            .directory
            .remove_pinned_regular_if_same(&file)
            .unwrap();
    }

    #[test]
    fn exact_initial_and_changed_bytes_are_bounded_owned_observations() {
        for bytes in [Vec::new(), vec![b'x'; MAX_Q_FIXTURE_BYTES as usize]] {
            let (_temporary, fixture) = make_fixture(&bytes);
            let captured = fixture.read_frozen_file(deadline()).unwrap();
            assert_eq!(captured.bytes, bytes);
            assert_eq!(captured.blob_hash, lillux::sha256_hex(&bytes));
            assert_eq!(captured.normalized_mode, ProjectFile::REGULAR_MODE);
        }
        let (_temporary, fixture) = make_fixture(b"before");
        let path = fixture.directory.path().join(&fixture.contract.leaf);
        std::fs::write(&path, b"after").unwrap();
        let captured = fixture.read_frozen_file(deadline()).unwrap();
        std::fs::write(&path, b"later").unwrap();
        assert_eq!(captured.bytes, b"after");
        assert_eq!(captured.blob_hash, lillux::sha256_hex(b"after"));
    }

    #[test]
    fn refuses_extra_missing_nested_and_linked_entries() {
        let (_temporary, fixture) = make_fixture(b"initial");
        fixture
            .directory
            .atomic_create_regular(OsStr::new("extra"), b"", 0o600)
            .unwrap();
        assert!(fixture.read_frozen_file(deadline()).is_err());

        let (_temporary, fixture) = make_fixture(b"initial");
        remove_leaf(&fixture);
        assert!(fixture.read_frozen_file(deadline()).is_err());
        fixture
            .directory
            .create_child(OsStr::new(&fixture.contract.leaf), 0o700)
            .unwrap();
        assert!(fixture.read_frozen_file(deadline()).is_err());

        let (temporary, fixture) = make_fixture(b"initial");
        let outside = temporary.path().join("outside");
        std::fs::write(&outside, b"private").unwrap();
        remove_leaf(&fixture);
        std::os::unix::fs::symlink(
            &outside,
            fixture.directory.path().join(&fixture.contract.leaf),
        )
        .unwrap();
        assert!(fixture.read_frozen_file(deadline()).is_err());
        assert_eq!(std::fs::read(&outside).unwrap(), b"private");

        let (temporary, fixture) = make_fixture(b"initial");
        std::fs::hard_link(
            fixture.directory.path().join(&fixture.contract.leaf),
            temporary.path().join("alias"),
        )
        .unwrap();
        assert!(fixture.read_frozen_file(deadline()).is_err());
    }

    #[test]
    fn refuses_executable_special_mode_oversize_and_special_file() {
        let (_temporary, fixture) = make_fixture(b"initial");
        let file = fixture
            .directory
            .open_pinned_regular(OsStr::new(&fixture.contract.leaf), false)
            .unwrap()
            .unwrap();
        for mode in [0o755, 0o4644, 0o2644, 0o1644] {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(
                fixture.directory.path().join(&fixture.contract.leaf),
                std::fs::Permissions::from_mode(mode),
            )
            .unwrap();
            assert!(fixture.read_frozen_file(deadline()).is_err());
        }
        file.set_mode(0o600).unwrap();
        assert!(fixture.read_frozen_file(deadline()).is_ok());
        std::fs::write(
            fixture.directory.path().join(&fixture.contract.leaf),
            vec![0; MAX_Q_FIXTURE_BYTES as usize + 1],
        )
        .unwrap();
        assert!(fixture.read_frozen_file(deadline()).is_err());

        let (_temporary, fixture) = make_fixture(b"initial");
        remove_leaf(&fixture);
        let _socket = std::os::unix::net::UnixListener::bind(
            fixture.directory.path().join(&fixture.contract.leaf),
        )
        .unwrap();
        assert!(fixture.read_frozen_file(deadline()).is_err());
    }

    #[test]
    fn refuses_replaced_original_directory_and_expired_deadline() {
        let (temporary, fixture) = make_fixture(b"initial");
        let original = fixture.directory.path().to_owned();
        std::fs::rename(&original, temporary.path().join("moved")).unwrap();
        std::fs::create_dir(&original).unwrap();
        std::fs::write(original.join(&fixture.contract.leaf), b"initial").unwrap();
        assert!(fixture.read_frozen_file(deadline()).is_err());

        let (_temporary, fixture) = make_fixture(b"initial");
        let expired = MonotonicDeadline::after(lillux::time::Duration::ZERO);
        assert!(fixture.read_frozen_file(expired).is_err());
    }
}
