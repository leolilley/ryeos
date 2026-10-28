//! Durable identity of the filesystem that holds an open descriptor.
//!
//! `st_dev` is the kernel's attachment number for a mounted filesystem, not a
//! property of the filesystem: the same disk re-attached by a platform gets a
//! different number. A persisted directory or file identity therefore names
//! the filesystem's own identity together with the inode. The value is
//! opaque: callers may store and compare it, never interpret it.
//!
//! The source is chosen by filesystem type alone, never by kernel version, so
//! a kernel upgrade never changes an identity:
//!
//! - ext2/3/4 and btrfs: the `statfs` filesystem id. These filesystems derive
//!   it from the UUID stored on disk, on every kernel, so it survives the disk
//!   being re-attached under another device number. btrfs also folds the
//!   subvolume's root id into it, so the subvolumes of one filesystem (which
//!   share a UUID, and whose roots are all inode 256) stay distinct.
//! - every other filesystem: the device number of the descriptor, which holds
//!   only while that attachment lasts (xfs, for one, derives its `statfs` id
//!   from the device, so it offers nothing more durable).
//!
//! `fstatfs` works on path-only descriptors, so a pin or an inherited handle
//! answers for itself.

use std::fs::File;

use anyhow::{Context, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Repr {
    /// A `statfs` filesystem id derived from the on-disk UUID.
    Fsid(u64),
    Device(u64),
}

/// Opaque, serializable identity of a filesystem, produced only from an open
/// descriptor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "Wire", into = "Wire")]
pub struct FilesystemIdentity(Repr);

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(untagged, deny_unknown_fields)]
enum Wire {
    /// Sixteen lowercase hexadecimal digits.
    Fsid {
        fsid: String,
    },
    Device {
        device: u64,
    },
}

impl From<FilesystemIdentity> for Wire {
    fn from(identity: FilesystemIdentity) -> Self {
        match identity.0 {
            Repr::Fsid(fsid) => Wire::Fsid {
                fsid: format!("{fsid:016x}"),
            },
            Repr::Device(device) => Wire::Device { device },
        }
    }
}

impl TryFrom<Wire> for FilesystemIdentity {
    type Error = String;

    fn try_from(wire: Wire) -> std::result::Result<Self, Self::Error> {
        match wire {
            Wire::Device { device } => Ok(Self(Repr::Device(device))),
            Wire::Fsid { fsid } => {
                if fsid.len() != 16
                    || !fsid
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
                {
                    return Err(
                        "filesystem id is not sixteen lowercase hexadecimal digits".to_owned()
                    );
                }
                let value = u64::from_str_radix(&fsid, 16).map_err(|error| error.to_string())?;
                if value == 0 {
                    return Err("filesystem id of zero names nothing".to_owned());
                }
                Ok(Self(Repr::Fsid(value)))
            }
        }
    }
}

/// `statfs` magic numbers of the filesystem types whose filesystem id is
/// derived from their on-disk UUID: ext2/3/4 (one magic) and btrfs.
const UUID_DERIVED_FSID_TYPES: [u32; 2] = [0xef53, 0x9123_683e];

/// Choose the identity from a filesystem type, its `statfs` id and the
/// descriptor's own `st_dev`. Deterministic by filesystem type; an id of zero
/// names nothing and falls back to the device number.
pub(crate) fn select_filesystem_identity(
    filesystem_type: u32,
    fsid: u64,
    st_dev: u64,
) -> FilesystemIdentity {
    if UUID_DERIVED_FSID_TYPES.contains(&filesystem_type) && fsid != 0 {
        FilesystemIdentity(Repr::Fsid(fsid))
    } else {
        FilesystemIdentity(Repr::Device(st_dev))
    }
}

/// Whether a durable identity survives the same data being re-attached under
/// another device number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IdentityStability {
    /// Named by the filesystem's own UUID-derived id.
    ReattachStable,
    /// The filesystem type offers no UUID-derived id, so the identity is the
    /// kernel's device number and holds only while this attachment lasts.
    AttachmentBound,
}

impl FilesystemIdentity {
    /// Whether this identity survives a re-attach. This is a report of what
    /// was discovered, not a way to read the identity.
    pub fn stability(&self) -> IdentityStability {
        match self.0 {
            Repr::Fsid(_) => IdentityStability::ReattachStable,
            Repr::Device(_) => IdentityStability::AttachmentBound,
        }
    }

    /// Identity of the filesystem holding `file`, which may be a path-only
    /// descriptor.
    pub(crate) fn of_descriptor(file: &File) -> Result<Self> {
        #[cfg(not(unix))]
        {
            let _ = file;
            anyhow::bail!("filesystem identity is unavailable on this platform");
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;
            let st_dev = file.metadata()?.dev();
            let (filesystem_type, fsid) = filesystem_type_and_id(file)?;
            Ok(select_filesystem_identity(filesystem_type, fsid, st_dev))
        }
    }

    /// Whether this names a filesystem at all: a device number of zero never
    /// does.
    pub fn is_named(&self) -> bool {
        !matches!(self.0, Repr::Device(0))
    }
}

/// The `statfs` filesystem type (low 32 bits: the magic numbers are 32-bit,
/// while the field's width and signedness differ by target) and filesystem id
/// of an open descriptor.
#[cfg(target_os = "linux")]
fn filesystem_type_and_id(file: &File) -> Result<(u32, u64)> {
    use std::os::fd::AsRawFd as _;
    let mut stats = std::mem::MaybeUninit::<libc::statfs>::zeroed();
    // SAFETY: the descriptor is open for the call and the kernel fully writes
    // the exclusively borrowed output on success.
    if unsafe { libc::fstatfs(file.as_raw_fd(), stats.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error())
            .context("read filesystem type of open descriptor");
    }
    // SAFETY: fstatfs succeeded.
    let kind = i128::from(unsafe { stats.assume_init() }.f_type) & 0xffff_ffff;
    let filesystem_type =
        u32::try_from(kind).context("filesystem type exceeds its 32-bit magic number")?;
    let mut vfs = std::mem::MaybeUninit::<libc::statvfs>::zeroed();
    // SAFETY: as above, for the statvfs view of the same descriptor, which
    // exposes the filesystem id as one integer.
    if unsafe { libc::fstatvfs(file.as_raw_fd(), vfs.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error())
            .context("read filesystem id of open descriptor");
    }
    // SAFETY: fstatvfs succeeded. The field's width differs by target;
    // widening through i128 is lossless everywhere.
    let fsid = u64::try_from(i128::from(unsafe { vfs.assume_init() }.f_fsid))
        .context("filesystem id is negative")?;
    Ok((filesystem_type, fsid))
}

/// Outside Linux no filesystem type has a UUID-derived id here: every
/// identity is the device number.
#[cfg(all(unix, not(target_os = "linux")))]
fn filesystem_type_and_id(_file: &File) -> Result<(u32, u64)> {
    Ok((0, 0))
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXT4: u32 = 0xef53;
    const BTRFS: u32 = 0x9123_683e;
    const XFS: u32 = 0x5846_5342;
    const TMPFS: u32 = 0x0102_1994;

    #[test]
    fn same_fsid_with_different_device_is_the_same_filesystem() {
        for kind in [EXT4, BTRFS] {
            assert_eq!(
                select_filesystem_identity(kind, 0xfeed, 100),
                select_filesystem_identity(kind, 0xfeed, 200)
            );
            assert_eq!(
                select_filesystem_identity(kind, 0xfeed, 100).stability(),
                IdentityStability::ReattachStable
            );
        }
    }

    #[test]
    fn different_fsid_is_a_different_filesystem() {
        assert_ne!(
            select_filesystem_identity(EXT4, 1, 100),
            select_filesystem_identity(EXT4, 2, 100)
        );
        // Two btrfs subvolumes: same UUID, different root ids folded into the
        // filesystem id.
        assert_ne!(
            select_filesystem_identity(BTRFS, 0x1234 ^ 5, 100),
            select_filesystem_identity(BTRFS, 0x1234 ^ 256, 100)
        );
    }

    #[test]
    fn other_types_and_a_zero_fsid_use_the_device_number() {
        for kind in [XFS, TMPFS] {
            let a = select_filesystem_identity(kind, 0xfeed, 7);
            assert_eq!(a, select_filesystem_identity(kind, 0xbeef, 7));
            assert_ne!(a, select_filesystem_identity(kind, 0xfeed, 8));
            assert_eq!(a.stability(), IdentityStability::AttachmentBound);
        }
        assert_eq!(
            select_filesystem_identity(EXT4, 0, 7),
            select_filesystem_identity(TMPFS, 0, 7)
        );
        assert!(!select_filesystem_identity(TMPFS, 0, 0).is_named());
        assert!(select_filesystem_identity(EXT4, 9, 0).is_named());
    }

    #[test]
    fn serialization_round_trips_and_refuses_bad_shapes() {
        let fsid = select_filesystem_identity(EXT4, 0x00ab_cdef_0123_4567, 3);
        let text = serde_json::to_string(&fsid).unwrap();
        assert_eq!(text, r#"{"fsid":"00abcdef01234567"}"#);
        assert_eq!(
            serde_json::from_str::<FilesystemIdentity>(&text).unwrap(),
            fsid
        );
        let device = select_filesystem_identity(TMPFS, 0, 42);
        let text = serde_json::to_string(&device).unwrap();
        assert_eq!(text, r#"{"device":42}"#);
        assert_eq!(
            serde_json::from_str::<FilesystemIdentity>(&text).unwrap(),
            device
        );
        for bad in [
            r#"{"fsid":"00ABCDEF01234567"}"#,
            r#"{"fsid":"abc"}"#,
            r#"{"fsid":"0000000000000000"}"#,
            r#"{"fsid":"00abcdef01234567","device":1}"#,
            r#"{"uuid":"00abcdef01234567"}"#,
            r#"{}"#,
        ] {
            assert!(
                serde_json::from_str::<FilesystemIdentity>(bad).is_err(),
                "{bad}"
            );
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_path_only_descriptor_has_the_same_identity_as_a_real_one() {
        use std::os::fd::FromRawFd as _;
        let open_path = |path: &std::ffi::CStr, flags: i32| {
            // SAFETY: the path is NUL-terminated; the descriptor returned is
            // new and owned by the File built from it.
            let descriptor = unsafe { libc::open(path.as_ptr(), flags | libc::O_CLOEXEC) };
            assert!(descriptor >= 0, "open {path:?}");
            unsafe { File::from_raw_fd(descriptor) }
        };
        let directory = tempfile::tempdir().unwrap();
        let path = std::ffi::CString::new(directory.path().to_str().unwrap()).unwrap();
        assert_eq!(
            FilesystemIdentity::of_descriptor(&open_path(&path, libc::O_PATH | libc::O_DIRECTORY))
                .unwrap(),
            FilesystemIdentity::of_descriptor(&File::open(directory.path()).unwrap()).unwrap()
        );
        let file_path = directory.path().join("regular");
        std::fs::write(&file_path, b"x").unwrap();
        let c_file = std::ffi::CString::new(file_path.to_str().unwrap()).unwrap();
        assert_eq!(
            FilesystemIdentity::of_descriptor(&open_path(&c_file, libc::O_PATH | libc::O_NOFOLLOW))
                .unwrap(),
            FilesystemIdentity::of_descriptor(&File::open(&file_path).unwrap()).unwrap()
        );
        // procfs has no UUID-derived id: its identity is the device number.
        let proc_identity = FilesystemIdentity::of_descriptor(&open_path(
            c"/proc",
            libc::O_PATH | libc::O_DIRECTORY,
        ))
        .unwrap();
        assert_eq!(
            proc_identity.stability(),
            IdentityStability::AttachmentBound
        );
        assert_eq!(
            proc_identity,
            FilesystemIdentity::of_descriptor(&File::open("/proc").unwrap()).unwrap()
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn pinned_identities_bind_the_filesystem_and_the_inode() {
        use crate::secure_fs::{PinnedDirectory, pinned_regular_file_identity};
        let directory = tempfile::tempdir().unwrap();
        let root = PinnedDirectory::open(directory.path()).unwrap().unwrap();
        let again = PinnedDirectory::open(directory.path()).unwrap().unwrap();
        let child = root
            .create_child(std::ffi::OsStr::new("child"), 0o700)
            .unwrap();
        assert_eq!(root.identity().unwrap(), again.identity().unwrap());
        assert_ne!(root.identity().unwrap(), child.identity().unwrap());
        assert!(
            root.identity()
                .unwrap()
                .same_filesystem(&child.identity().unwrap())
        );
        std::fs::write(directory.path().join("a"), b"a").unwrap();
        std::fs::write(directory.path().join("b"), b"b").unwrap();
        let a =
            pinned_regular_file_identity(&File::open(directory.path().join("a")).unwrap()).unwrap();
        let b =
            pinned_regular_file_identity(&File::open(directory.path().join("b")).unwrap()).unwrap();
        assert_ne!(a, b);
        assert!(a.same_filesystem(&b));
        assert_eq!(a.filesystem(), root.identity().unwrap().filesystem());
    }
}
