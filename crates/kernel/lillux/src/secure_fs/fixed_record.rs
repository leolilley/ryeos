//! Exact, caller-bounded records in an already owned inode. Lillux performs
//! positional I/O and durability; the caller owns the record schema, exclusion
//! and durable recovery protocol. A failed overwrite can leave a target prefix.

use super::{PinnedDirectory, PinnedRegularFile, PinnedRegularFileIdentity};
use crate::time::MonotonicDeadline;
use anyhow::{Context, Result, ensure};

impl PinnedRegularFile {
    pub fn identity(&self) -> Result<PinnedRegularFileIdentity> {
        super::pinned_regular_file_identity(&self.file)
    }

    /// Check ownership of this exact regular inode by the effective user.
    /// The parent remains a separate namespace authority.
    pub fn require_effective_user_owned(&self) -> Result<()> {
        let owner = super::require_effective_user_owned_regular(&self.file)?;
        self.require_owner_fixed(owner.owner()).map_err(anyhow::Error::msg)
    }

    /// Read a fixed record without changing a shared descriptor cursor. The
    /// caller supplies all payload storage and the original absolute deadline.
    pub fn read_exact_stable_into(&self, bytes: &mut [u8], original: MonotonicDeadline) -> Result<()> {
        ensure!(!bytes.is_empty(), "fixed record buffer is empty");
        ensure!(!original.has_elapsed(), "fixed record read interval exhausted");
        let before = self.observation()?;
        ensure!(before.size() == u64::try_from(bytes.len())?, "fixed record size changed");
        let mut offset = 0;
        while offset < bytes.len() {
            ensure!(!original.has_elapsed(), "fixed record read interval exhausted");
            #[cfg(unix)]
            let read = std::os::unix::fs::FileExt::read_at(&self.file, &mut bytes[offset..], u64::try_from(offset)?);
            #[cfg(windows)]
            let read = std::os::windows::fs::FileExt::seek_read(&self.file, &mut bytes[offset..], u64::try_from(offset)?);
            #[cfg(not(any(unix, windows)))]
            let read: std::io::Result<usize> = Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported, "positional fixed record reads are unavailable"));
            match read {
                Ok(0) => anyhow::bail!("fixed record ended before its retained length"),
                Ok(count) => offset += count,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error).context("read exact retained record"),
            }
        }
        super::ensure_open_regular_file_unchanged(&self.file, &before)?;
        ensure!(!original.has_elapsed(), "fixed record read finished after its interval");
        Ok(())
    }

    /// Conditional in-place write of an unchanged-length record. The caller
    /// must hold namespace/writer exclusion and durably retain the permitted
    /// predecessor/target BEFORE calling this. There is no truncate, rename,
    /// rollback or schema interpretation. Error retains the same borrowed
    /// inode; a partial write or uncertain sync needs the caller's exact redo.
    pub fn overwrite_fixed_if_matches(
        &self, expected: &[u8], target: &[u8], scratch: &mut [u8],
        original: MonotonicDeadline,
    ) -> Result<()> {
        ensure!(!target.is_empty() && expected.len() == target.len() && scratch.len() == target.len(),
            "fixed record write lengths differ or are empty");
        self.read_exact_stable_into(scratch, original)?;
        ensure!(scratch == expected, "fixed record predecessor changed");
        #[cfg(unix)]
        {
            use std::os::unix::fs::FileExt as _;
            let mut offset = 0;
            while offset < target.len() {
                ensure!(!original.has_elapsed(), "fixed record write interval exhausted");
                match self.file.write_at(&target[offset..], u64::try_from(offset)?) {
                    Ok(0) => anyhow::bail!("fixed record write made no progress"),
                    Ok(count) => offset += count,
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(error) => return Err(error).context("write retained fixed record"),
                }
            }
            self.sync_exact(original)?;
            self.read_exact_stable_into(scratch, original)?;
            ensure!(scratch == target, "fixed record target readback changed");
            Ok(())
        }
        #[cfg(not(unix))]
        anyhow::bail!("positional fixed record writing is unavailable on this platform")
    }

    /// Durability barrier on this actual owned inode. A late/error result
    /// does not imply that earlier writes or publication were undone.
    pub fn sync_exact(&self, original: MonotonicDeadline) -> Result<()> {
        ensure!(!original.has_elapsed(), "retained record sync interval exhausted");
        self.file.sync_all().context("sync retained record")?;
        ensure!(!original.has_elapsed(), "retained record sync completed after its interval");
        Ok(())
    }
}

impl PinnedDirectory {
    /// Typed counterpart to the fixed descriptor comparison; no descriptor
    /// escapes and the child's retained coordinate cannot be substituted.
    pub fn require_pinned_regular_entry_fixed(&self, expected: &PinnedRegularFile)
        -> std::result::Result<(), &'static str> {
        self.require_regular_entry_descriptor_fixed(expected.name(), &expected.file)
    }
}

#[cfg(all(test, target_os = "linux"))]
mod controls {
    use super::*;
    use std::ffi::OsStr;

    #[test]
    fn fixed_overwrite_keeps_inode_and_refuses_changed_bytes_size_and_expiry() {
        let fixture = tempfile::tempdir().unwrap();
        let directory = PinnedDirectory::open(fixture.path()).unwrap().unwrap();
        let initial = [0x31; 64];
        directory.atomic_create_regular(
            OsStr::new("record"),
            &initial,
            0o600,
            crate::secure_fs::TEST_TEMP_PURPOSE,
        ).unwrap().unwrap();
        let pin = directory.open_pinned_regular(OsStr::new("record"), true).unwrap().unwrap();
        let identity = pin.identity().unwrap();
        let deadline = MonotonicDeadline::after(crate::time::Duration::from_secs(5));
        let mut scratch = [0; 64];
        let target = [0x42; 64];
        pin.overwrite_fixed_if_matches(&initial, &target, &mut scratch, deadline).unwrap();
        assert_eq!(pin.identity().unwrap(), identity);
        directory.require_pinned_regular_entry_fixed(&pin).unwrap();
        assert!(pin.overwrite_fixed_if_matches(&initial, &initial, &mut scratch, deadline).is_err());
        assert!(pin.overwrite_fixed_if_matches(&target, &[1; 63], &mut scratch, deadline).is_err());
        assert!(pin.overwrite_fixed_if_matches(&target, &initial, &mut scratch,
            MonotonicDeadline::after(crate::time::Duration::ZERO)).is_err());
        pin.read_exact_stable_into(&mut scratch, deadline).unwrap();
        assert_eq!(scratch, target);
    }
}
