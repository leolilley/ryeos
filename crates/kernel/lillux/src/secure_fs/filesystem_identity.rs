//! Durable identity of the filesystem that holds an open descriptor.
//!
//! `st_dev` is the kernel's attachment number for a mounted filesystem, not a
//! property of the filesystem: the same disk re-attached by a platform gets a
//! different number. A persisted directory or file identity therefore names the
//! filesystem's own identity together with the inode. The value is opaque:
//! callers may store and compare it, never interpret it.
//!
//! A btrfs filesystem shares one UUID across all its subvolumes and every
//! subvolume root is inode 256, so `{uuid, inode}` alone collides across
//! subvolumes. Where the kernel reports a subvolume id (`STATX_SUBVOL`), it is
//! part of the identity; where it does not, no subvolume is recorded.

use std::fs::File;

use anyhow::{Context, Result, bail};

/// Longest filesystem UUID the kernel reports.
const MAX_UUID_BYTES: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Repr {
    Uuid {
        len: u8,
        bytes: [u8; MAX_UUID_BYTES],
        subvolume: Option<u64>,
    },
    Device(u64),
}

/// Opaque, serializable identity of a filesystem, produced only from an open
/// descriptor. It is the filesystem's own UUID when the kernel reports one and
/// otherwise the raw device number of that same descriptor, which is only
/// meaningful while the filesystem stays attached.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "Wire", into = "Wire")]
pub struct FilesystemIdentity(Repr);

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(untagged, deny_unknown_fields)]
enum Wire {
    /// `subvolume` is omitted when the kernel reported none, so the keys stay
    /// in sorted order (`subvolume` before `uuid`) for canonical digests.
    Uuid {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        subvolume: Option<u64>,
        uuid: String,
    },
    Device {
        device: u64,
    },
}

impl From<FilesystemIdentity> for Wire {
    fn from(identity: FilesystemIdentity) -> Self {
        match identity.0 {
            Repr::Uuid {
                len,
                bytes,
                subvolume,
            } => {
                use std::fmt::Write as _;
                let mut uuid = String::with_capacity(usize::from(len) * 2);
                for byte in &bytes[..usize::from(len)] {
                    // Writing to a String cannot fail.
                    let _ = write!(uuid, "{byte:02x}");
                }
                Wire::Uuid { subvolume, uuid }
            }
            Repr::Device(device) => Wire::Device { device },
        }
    }
}

impl TryFrom<Wire> for FilesystemIdentity {
    type Error = String;

    fn try_from(wire: Wire) -> std::result::Result<Self, Self::Error> {
        match wire {
            Wire::Device { device } => Ok(Self(Repr::Device(device))),
            Wire::Uuid { subvolume, uuid } => {
                let text = uuid.as_bytes();
                if text.is_empty() || text.len() > MAX_UUID_BYTES * 2 || text.len() % 2 != 0 {
                    return Err("filesystem uuid has an invalid length".to_owned());
                }
                let mut bytes = [0_u8; MAX_UUID_BYTES];
                for (slot, pair) in bytes.iter_mut().zip(text.chunks_exact(2)) {
                    *slot = (lower_hex(pair[0])? << 4) | lower_hex(pair[1])?;
                }
                Ok(Self(Repr::Uuid {
                    len: u8::try_from(text.len() / 2)
                        .map_err(|_| "filesystem uuid has an invalid length".to_owned())?,
                    bytes,
                    subvolume,
                }))
            }
        }
    }
}

fn lower_hex(digit: u8) -> std::result::Result<u8, String> {
    match digit {
        b'0'..=b'9' => Ok(digit - b'0'),
        b'a'..=b'f' => Ok(digit - b'a' + 10),
        _ => Err("filesystem uuid is not lowercase hexadecimal".to_owned()),
    }
}

/// The outcome of asking the kernel for a filesystem UUID.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UuidProbe {
    /// The kernel reported `len` significant bytes.
    Reported {
        len: u8,
        bytes: [u8; MAX_UUID_BYTES],
    },
    /// The call failed with this errno.
    Failed(i32),
}

/// What the kernel says about a subvolume id for a descriptor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SubvolumeProbe {
    /// The filesystem type has no subvolumes (anything but btrfs and
    /// bcachefs): no subvolume is ever recorded, whatever `statx` returns.
    NotApplicable,
    /// A subvolume filesystem whose kernel reported this id.
    Reported(u64),
    /// A subvolume filesystem whose kernel did not report an id (older than
    /// 6.10). The UUID alone would collide across subvolumes, so the identity
    /// falls back to the attachment-bound device number.
    Unavailable,
}

/// Choose the identity from a probe outcome and the descriptor's own
/// `st_dev`. A filesystem without a UUID (or a kernel without the call, or a
/// caller the kernel refuses it to) falls back to the device number, as does
/// an all-zero or all-ones UUID, which names nothing; any other failure is an
/// error. The result is deterministic per filesystem type: a subvolume id is
/// part of the identity on subvolume filesystems only.
pub(crate) fn select_filesystem_identity(
    probe: UuidProbe,
    st_dev: u64,
    subvolume: SubvolumeProbe,
) -> Result<FilesystemIdentity> {
    match probe {
        UuidProbe::Reported { len, bytes } => {
            if len == 0 || usize::from(len) > MAX_UUID_BYTES {
                bail!("kernel reported a filesystem uuid of invalid length {len}");
            }
            let significant = &bytes[..usize::from(len)];
            if significant.iter().all(|byte| *byte == 0)
                || significant.iter().all(|byte| *byte == 0xff)
            {
                return Ok(FilesystemIdentity(Repr::Device(st_dev)));
            }
            let subvolume = match subvolume {
                SubvolumeProbe::NotApplicable => None,
                SubvolumeProbe::Reported(id) => Some(id),
                SubvolumeProbe::Unavailable => {
                    return Ok(FilesystemIdentity(Repr::Device(st_dev)));
                }
            };
            let mut canonical = [0_u8; MAX_UUID_BYTES];
            canonical[..usize::from(len)].copy_from_slice(significant);
            Ok(FilesystemIdentity(Repr::Uuid {
                len,
                bytes: canonical,
                subvolume,
            }))
        }
        UuidProbe::Failed(errno) => {
            if uuid_unsupported(errno) {
                Ok(FilesystemIdentity(Repr::Device(st_dev)))
            } else {
                Err(std::io::Error::from_raw_os_error(errno))
                    .context("read filesystem uuid of open descriptor")
            }
        }
    }
}

#[cfg(unix)]
fn uuid_unsupported(errno: i32) -> bool {
    matches!(
        errno,
        libc::ENOTTY | libc::EOPNOTSUPP | libc::ENOSYS | libc::EINVAL | libc::EPERM | libc::EACCES
    )
}

#[cfg(not(unix))]
fn uuid_unsupported(_errno: i32) -> bool {
    false
}

/// Whether a durable identity survives the same data being re-attached under
/// another device number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IdentityStability {
    /// Named by the filesystem's own UUID.
    ReattachStable,
    /// Named by the filesystem's own UUID and the subvolume id the kernel
    /// reported, which separates the subvolumes of a btrfs or bcachefs
    /// filesystem that share a UUID. Other filesystems never carry one.
    ReattachStableWithSubvolume,
    /// No filesystem UUID was available, so the identity is the kernel's
    /// device number and holds only while this attachment lasts.
    AttachmentBound,
}

impl FilesystemIdentity {
    /// Whether this identity survives a re-attach. This is a report of what
    /// was discovered, not a way to read the identity.
    pub fn stability(&self) -> IdentityStability {
        match self.0 {
            Repr::Uuid {
                subvolume: Some(_), ..
            } => IdentityStability::ReattachStableWithSubvolume,
            Repr::Uuid {
                subvolume: None, ..
            } => IdentityStability::ReattachStable,
            Repr::Device(_) => IdentityStability::AttachmentBound,
        }
    }

    /// Identity of the filesystem holding `file`.
    pub(crate) fn of_descriptor(file: &File) -> Result<Self> {
        #[cfg(not(unix))]
        {
            let _ = file;
            bail!("filesystem identity is unavailable on this platform");
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;
            let st_dev = file.metadata()?.dev();
            // The UUID ioctl needs a real descriptor. A path-only descriptor
            // (an exact process root, an inherited directory handle, a pin)
            // is reopened first; any other failure of the query stays an
            // error.
            let reopened = reopen_path_only(file)?;
            let queried = reopened.as_ref().unwrap_or(file);
            select_filesystem_identity(probe_uuid(queried), st_dev, subvolume_probe(queried)?)
        }
    }

    /// Whether this names a filesystem at all: a device number of zero never
    /// does.
    pub fn is_named(&self) -> bool {
        !matches!(self.0, Repr::Device(0))
    }

    /// The identity of the filesystem holding `entry`, a child of `parent`,
    /// when `entry` is a path-only descriptor from which the UUID cannot be
    /// read. The UUID comes from `parent`; the subvolume is read from the
    /// entry's own descriptor and must equal the parent's, because a
    /// different subvolume is a different identity.
    pub(crate) fn of_parent_and_entry(parent: &File, entry: &File) -> Result<Self> {
        #[cfg(not(unix))]
        {
            let _ = (parent, entry);
            bail!("filesystem identity is unavailable on this platform");
        }
        #[cfg(unix)]
        {
            let identity = Self::of_descriptor(parent)?;
            if let Repr::Uuid { subvolume, .. } = identity.0
                && subvolume_probe(entry)?
                    != subvolume.map_or(SubvolumeProbe::NotApplicable, SubvolumeProbe::Reported)
            {
                bail!("entry is in another subvolume than its parent directory");
            }
            Ok(identity)
        }
    }
}

/// A real read-only descriptor for `file` when it is a path-only (`O_PATH`)
/// descriptor, else `None`; the filesystem UUID ioctl needs a real one. A
/// directory is reopened relative to itself. Anything else is reopened
/// through its own `/proc/self/fd` entry, which names the same inode, and the
/// result is refused unless it still is that inode. A descriptor that cannot
/// be reopened (for instance a file nobody may read) is an error, never a
/// silent fallback.
#[cfg(target_os = "linux")]
fn reopen_path_only(file: &File) -> Result<Option<File>> {
    use std::os::fd::{AsRawFd as _, FromRawFd as _};
    use std::os::unix::fs::MetadataExt as _;
    // SAFETY: F_GETFL only reads the descriptor's status flags.
    let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) };
    if flags < 0 {
        return Err(std::io::Error::last_os_error()).context("read descriptor status flags");
    }
    if flags & libc::O_PATH == 0 {
        return Ok(None);
    }
    let metadata = file.metadata()?;
    // SAFETY: both calls name the inode the descriptor itself holds and return
    // a new descriptor that nothing else owns.
    let descriptor = unsafe {
        if metadata.is_dir() {
            libc::openat(
                file.as_raw_fd(),
                c".".as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
            )
        } else {
            let path = std::ffi::CString::new(format!("/proc/self/fd/{}", file.as_raw_fd()))?;
            libc::open(
                path.as_ptr(),
                libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NONBLOCK | libc::O_NOCTTY,
            )
        }
    };
    if descriptor < 0 {
        return Err(std::io::Error::last_os_error())
            .context("reopen path-only descriptor for its filesystem identity");
    }
    // SAFETY: open returned a new descriptor that nothing else owns.
    let reopened = unsafe { File::from_raw_fd(descriptor) };
    let after = reopened.metadata()?;
    if (after.dev(), after.ino()) != (metadata.dev(), metadata.ino()) {
        bail!("path-only descriptor changed identity while it was reopened");
    }
    Ok(Some(reopened))
}

#[cfg(all(unix, not(target_os = "linux")))]
fn reopen_path_only(_file: &File) -> Result<Option<File>> {
    Ok(None)
}

/// `STATX_SUBVOL` (Linux 6.10).
#[cfg(target_os = "linux")]
const STATX_SUBVOL: u32 = 0x8000;
/// Byte offsets inside `struct statx`: `stx_mask` and `stx_subvol`.
#[cfg(target_os = "linux")]
const STATX_MASK_OFFSET: usize = 0;
#[cfg(target_os = "linux")]
const STATX_SUBVOL_OFFSET: usize = 0xa0;
#[cfg(target_os = "linux")]
const STATX_BYTES: usize = 256;

/// btrfs and bcachefs `statfs` magic numbers: the filesystem types with
/// subvolumes.
#[cfg(target_os = "linux")]
const SUBVOLUME_FILESYSTEMS: [i64; 2] = [0x9123_683e, 0xca45_1a4e];

/// The subvolume id a `statx` result carries, only when its mask has the bit.
#[cfg(target_os = "linux")]
pub(crate) fn subvolume_from_statx(mask: u32, value: u64) -> Option<u64> {
    const SUBVOL_BIT: u32 = 0x8000;
    (mask & SUBVOL_BIT != 0).then_some(value)
}

/// Whether `file` is on a filesystem type that has subvolumes (`fstatfs`
/// works on path-only descriptors).
#[cfg(target_os = "linux")]
fn on_subvolume_filesystem(file: &File) -> Result<bool> {
    use std::os::fd::AsRawFd as _;
    let mut stats = std::mem::MaybeUninit::<libc::statfs>::zeroed();
    // SAFETY: the descriptor is open for the call and the kernel fully writes
    // the exclusively borrowed output on success.
    if unsafe { libc::fstatfs(file.as_raw_fd(), stats.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error())
            .context("read filesystem type of open descriptor");
    }
    // SAFETY: fstatfs succeeded.
    let kind = unsafe { stats.assume_init() }.f_type;
    // `f_type`'s integer width and signedness differ by target, while the
    // magic numbers are 32-bit; compare the low 32 bits.
    let kind = i128::from(kind) & 0xffff_ffff;
    Ok(SUBVOLUME_FILESYSTEMS
        .iter()
        .any(|magic| i128::from(*magic) == kind))
}

/// The subvolume disposition of an open descriptor, deterministic by
/// filesystem type.
#[cfg(target_os = "linux")]
fn subvolume_probe(file: &File) -> Result<SubvolumeProbe> {
    if !on_subvolume_filesystem(file)? {
        return Ok(SubvolumeProbe::NotApplicable);
    }
    Ok(subvolume_of(file)?.map_or(SubvolumeProbe::Unavailable, SubvolumeProbe::Reported))
}

#[cfg(all(unix, not(target_os = "linux")))]
fn subvolume_probe(_file: &File) -> Result<SubvolumeProbe> {
    Ok(SubvolumeProbe::NotApplicable)
}

/// The subvolume of an open descriptor, including a path-only one, or `None`
/// when the kernel reports none.
#[cfg(target_os = "linux")]
fn subvolume_of(file: &File) -> Result<Option<u64>> {
    use std::os::fd::AsRawFd as _;
    let mut buffer = [0_u64; STATX_BYTES / 8];
    // SAFETY: the descriptor is open for the call, the empty path with
    // AT_EMPTY_PATH names the descriptor itself, and the kernel writes at most
    // STATX_BYTES bytes into the exclusively borrowed buffer.
    let status = unsafe {
        libc::syscall(
            libc::SYS_statx,
            file.as_raw_fd(),
            c"".as_ptr(),
            libc::AT_EMPTY_PATH,
            STATX_SUBVOL,
            buffer.as_mut_ptr(),
        )
    };
    if status != 0 {
        let error = std::io::Error::last_os_error();
        return match error.raw_os_error() {
            Some(libc::ENOSYS | libc::EOPNOTSUPP | libc::EINVAL) => Ok(None),
            _ => Err(error).context("read subvolume of open descriptor"),
        };
    }
    // SAFETY: `buffer` is 256 initialized bytes; read the two fields at their
    // fixed `struct statx` offsets without alignment assumptions.
    let bytes = unsafe { std::slice::from_raw_parts(buffer.as_ptr().cast::<u8>(), STATX_BYTES) };
    let mask = u32::from_ne_bytes(bytes[STATX_MASK_OFFSET..STATX_MASK_OFFSET + 4].try_into()?);
    let value = u64::from_ne_bytes(bytes[STATX_SUBVOL_OFFSET..STATX_SUBVOL_OFFSET + 8].try_into()?);
    Ok(subvolume_from_statx(mask, value))
}

/// `struct fsuuid2 { __u8 len; __u8 uuid[16]; }`.
#[cfg(target_os = "linux")]
#[repr(C)]
struct FsUuid2 {
    len: u8,
    uuid: [u8; MAX_UUID_BYTES],
}

/// `_IOR(0x15, 0, struct fsuuid2)`.
#[cfg(target_os = "linux")]
const FS_IOC_GETFSUUID: u64 = 0x8011_1500;

#[cfg(target_os = "linux")]
fn probe_uuid(file: &File) -> UuidProbe {
    use std::os::fd::AsRawFd as _;
    let mut out = FsUuid2 {
        len: 0,
        uuid: [0; MAX_UUID_BYTES],
    };
    // SAFETY: the descriptor is open for the call and the request writes at
    // most `size_of::<FsUuid2>()` bytes into the exclusively borrowed `out`.
    let status = unsafe {
        libc::ioctl(
            file.as_raw_fd(),
            FS_IOC_GETFSUUID as libc::Ioctl,
            &mut out as *mut FsUuid2,
        )
    };
    if status == 0 {
        UuidProbe::Reported {
            len: out.len,
            bytes: out.uuid,
        }
    } else {
        UuidProbe::Failed(std::io::Error::last_os_error().raw_os_error().unwrap_or(0))
    }
}

#[cfg(all(unix, not(target_os = "linux")))]
fn probe_uuid(_file: &File) -> UuidProbe {
    UuidProbe::Failed(libc::ENOTTY)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn select(probe: UuidProbe, st_dev: u64) -> Result<FilesystemIdentity> {
        select_filesystem_identity(probe, st_dev, SubvolumeProbe::NotApplicable)
    }

    fn uuid(first: u8) -> UuidProbe {
        let mut bytes = [0; MAX_UUID_BYTES];
        bytes[0] = first;
        bytes[15] = 9;
        UuidProbe::Reported { len: 16, bytes }
    }

    #[test]
    fn same_uuid_with_different_device_is_the_same_filesystem() {
        let before = select(uuid(1), 100).unwrap();
        let after = select(uuid(1), 200).unwrap();
        assert_eq!(before, after);
    }

    #[test]
    fn different_uuid_is_a_different_filesystem() {
        let left = select(uuid(1), 100).unwrap();
        let right = select(uuid(2), 100).unwrap();
        assert_ne!(left, right);
    }

    #[test]
    fn missing_uuid_falls_back_to_the_device_number() {
        for errno in [libc::ENOTTY, libc::EOPNOTSUPP, libc::ENOSYS, libc::EINVAL] {
            let a = select(UuidProbe::Failed(errno), 7).unwrap();
            let b = select(UuidProbe::Failed(errno), 7).unwrap();
            let c = select(UuidProbe::Failed(errno), 8).unwrap();
            assert_eq!(a, b);
            assert_ne!(a, c);
        }
        assert_ne!(
            select(UuidProbe::Failed(libc::ENOTTY), 7).unwrap(),
            select(uuid(1), 7).unwrap()
        );
    }

    #[test]
    fn other_errors_and_invalid_lengths_are_refused() {
        assert!(select(UuidProbe::Failed(libc::EBADF), 7).is_err());
        assert!(select(UuidProbe::Failed(libc::EIO), 7).is_err());
        for len in [0_u8, 17] {
            let probe = UuidProbe::Reported {
                len,
                bytes: [1; MAX_UUID_BYTES],
            };
            assert!(select(probe, 7).is_err());
        }
    }

    #[test]
    fn bytes_beyond_the_reported_length_do_not_affect_equality() {
        let mut left = [0; MAX_UUID_BYTES];
        let mut right = [0; MAX_UUID_BYTES];
        left[0] = 5;
        right[0] = 5;
        right[10] = 77;
        let left = select(
            UuidProbe::Reported {
                len: 4,
                bytes: left,
            },
            1,
        );
        let right = select(
            UuidProbe::Reported {
                len: 4,
                bytes: right,
            },
            1,
        );
        assert_eq!(left.unwrap(), right.unwrap());
    }

    #[test]
    fn serialization_round_trips_and_refuses_unknown_shapes() {
        let identity = select(uuid(0xab), 1).unwrap();
        let text = serde_json::to_string(&identity).unwrap();
        assert!(text.contains("ab"), "{text}");
        assert_eq!(
            serde_json::from_str::<FilesystemIdentity>(&text).unwrap(),
            identity
        );
        let device = select(UuidProbe::Failed(libc::ENOTTY), 42).unwrap();
        let text = serde_json::to_string(&device).unwrap();
        assert_eq!(
            serde_json::from_str::<FilesystemIdentity>(&text).unwrap(),
            device
        );
        for bad in [
            r#"{"uuid":"AB"}"#,
            r#"{"uuid":""}"#,
            r#"{"uuid":"abc"}"#,
            r#"{"uuid":"ab","device":1}"#,
            r#"{"device":1,"extra":2}"#,
            r#"{"uuid":"00112233445566778899aabbccddeeff00"}"#,
        ] {
            assert!(
                serde_json::from_str::<FilesystemIdentity>(bad).is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn subvolume_roots_with_the_same_uuid_and_inode_are_different() {
        let root = |subvolume: SubvolumeProbe, dev: u64| {
            select_filesystem_identity(uuid(1), dev, subvolume).unwrap()
        };
        let reported = SubvolumeProbe::Reported;
        // Two btrfs subvolume roots: one UUID, inode 256 in each.
        assert_ne!(root(reported(256), 10), root(reported(257), 10));
        // A reattach keeps the UUID and subvolume and changes only st_dev.
        assert_eq!(root(reported(256), 10), root(reported(256), 99));
        // A reported subvolume never equals a filesystem without subvolumes.
        assert_ne!(
            root(reported(256), 10),
            root(SubvolumeProbe::NotApplicable, 10)
        );
    }

    #[test]
    fn identity_is_deterministic_by_filesystem_type() {
        // A subvolume filesystem whose kernel reports no id is attachment
        // bound: the UUID alone would collide across subvolumes.
        let unavailable =
            select_filesystem_identity(uuid(1), 10, SubvolumeProbe::Unavailable).unwrap();
        assert_eq!(unavailable.stability(), IdentityStability::AttachmentBound);
        assert_ne!(
            unavailable,
            select_filesystem_identity(uuid(1), 11, SubvolumeProbe::Unavailable).unwrap()
        );
        // Stability values follow the subvolume disposition.
        assert_eq!(
            select_filesystem_identity(uuid(1), 10, SubvolumeProbe::Reported(5))
                .unwrap()
                .stability(),
            IdentityStability::ReattachStableWithSubvolume
        );
        assert_eq!(
            select_filesystem_identity(uuid(1), 10, SubvolumeProbe::NotApplicable)
                .unwrap()
                .stability(),
            IdentityStability::ReattachStable
        );
    }

    #[test]
    fn an_empty_uuid_names_nothing_and_a_refused_ioctl_is_unsupported() {
        for bytes in [[0_u8; MAX_UUID_BYTES], [0xff; MAX_UUID_BYTES]] {
            for len in [1_u8, 16] {
                let probe = UuidProbe::Reported { len, bytes };
                let identity = select(probe, 9).unwrap();
                assert_eq!(identity.stability(), IdentityStability::AttachmentBound);
                assert_eq!(identity, select(probe, 9).unwrap());
                assert_ne!(identity, select(probe, 10).unwrap());
            }
        }
        for errno in [libc::EPERM, libc::EACCES] {
            assert_eq!(
                select(UuidProbe::Failed(errno), 3).unwrap().stability(),
                IdentityStability::AttachmentBound
            );
        }
        assert!(
            !select(UuidProbe::Failed(libc::ENOTTY), 0)
                .unwrap()
                .is_named()
        );
        assert!(
            select(UuidProbe::Failed(libc::ENOTTY), 1)
                .unwrap()
                .is_named()
        );
        assert!(select(uuid(1), 0).unwrap().is_named());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn subvolume_is_taken_only_when_the_mask_has_the_bit() {
        assert_eq!(subvolume_from_statx(0x8000 | 0x7ff, 5), Some(5));
        assert_eq!(subvolume_from_statx(0x7ff, 5), None);
    }

    #[test]
    fn subvolume_serialization_is_sorted_omitted_when_absent_and_strict() {
        let with =
            select_filesystem_identity(uuid(0xab), 1, SubvolumeProbe::Reported(256)).unwrap();
        let text = serde_json::to_string(&with).unwrap();
        assert!(text.starts_with(r#"{"subvolume":256,"uuid":""#), "{text}");
        assert_eq!(
            serde_json::from_str::<FilesystemIdentity>(&text).unwrap(),
            with
        );
        let without =
            select_filesystem_identity(uuid(0xab), 1, SubvolumeProbe::NotApplicable).unwrap();
        let text = serde_json::to_string(&without).unwrap();
        assert!(text.starts_with(r#"{"uuid":""#), "{text}");
        assert_ne!(with, without);
        assert!(
            serde_json::from_str::<FilesystemIdentity>(r#"{"subvolume":"x","uuid":"ab"}"#).is_err()
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_path_only_directory_and_procfs_have_identities() {
        use std::os::fd::FromRawFd as _;
        let open_path = |path: &std::ffi::CStr| {
            // SAFETY: the path is NUL-terminated; the descriptor returned is
            // new and owned by the File built from it.
            let descriptor = unsafe {
                libc::open(
                    path.as_ptr(),
                    libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC,
                )
            };
            assert!(descriptor >= 0, "open {path:?}");
            unsafe { File::from_raw_fd(descriptor) }
        };
        let directory = tempfile::tempdir().unwrap();
        let path = std::ffi::CString::new(directory.path().to_str().unwrap()).unwrap();
        let path_only = open_path(&path);
        let real = File::open(directory.path()).unwrap();
        assert_eq!(
            FilesystemIdentity::of_descriptor(&path_only).unwrap(),
            FilesystemIdentity::of_descriptor(&real).unwrap()
        );
        // procfs has no UUID: the ioctl is refused and the device number is
        // the identity, for a real and a path-only descriptor alike.
        let proc_path_only = open_path(c"/proc");
        let proc_real = File::open("/proc").unwrap();
        let proc_identity = FilesystemIdentity::of_descriptor(&proc_path_only).unwrap();
        assert_eq!(
            proc_identity.stability(),
            IdentityStability::AttachmentBound
        );
        assert_eq!(
            proc_identity,
            FilesystemIdentity::of_descriptor(&proc_real).unwrap()
        );
        // A path-only regular file resolves to the same identity as a real one.
        let file_path = directory.path().join("regular");
        std::fs::write(&file_path, b"x").unwrap();
        let c_file = std::ffi::CString::new(file_path.to_str().unwrap()).unwrap();
        // SAFETY: NUL-terminated path; the returned descriptor is owned below.
        let descriptor = unsafe {
            libc::open(
                c_file.as_ptr(),
                libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        assert!(descriptor >= 0);
        let path_only_file = unsafe { File::from_raw_fd(descriptor) };
        assert_eq!(
            FilesystemIdentity::of_descriptor(&path_only_file).unwrap(),
            FilesystemIdentity::of_descriptor(&File::open(&file_path).unwrap()).unwrap()
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_real_descriptor_has_a_stable_identity() {
        let directory = tempfile::tempdir().unwrap();
        let a = File::open(directory.path()).unwrap();
        let b = File::open(directory.path()).unwrap();
        assert_eq!(
            FilesystemIdentity::of_descriptor(&a).unwrap(),
            FilesystemIdentity::of_descriptor(&b).unwrap()
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
