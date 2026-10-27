//! Durable identity of the filesystem that holds an open descriptor.
//!
//! `st_dev` is the kernel's attachment number for a mounted filesystem, not a
//! property of the filesystem: the same disk re-attached by a platform gets a
//! different number. A persisted directory or file identity therefore names the
//! filesystem's own identity together with the inode. The value is opaque:
//! callers may store and compare it, never interpret it.

use std::fs::File;

use anyhow::{Context, Result, bail};

/// Longest filesystem UUID the kernel reports.
const MAX_UUID_BYTES: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Repr {
    Uuid {
        len: u8,
        bytes: [u8; MAX_UUID_BYTES],
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
    Uuid { uuid: String },
    Device { device: u64 },
}

impl From<FilesystemIdentity> for Wire {
    fn from(identity: FilesystemIdentity) -> Self {
        match identity.0 {
            Repr::Uuid { len, bytes } => {
                use std::fmt::Write as _;
                let mut uuid = String::with_capacity(usize::from(len) * 2);
                for byte in &bytes[..usize::from(len)] {
                    // Writing to a String cannot fail.
                    let _ = write!(uuid, "{byte:02x}");
                }
                Wire::Uuid { uuid }
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
            Wire::Uuid { uuid } => {
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

/// Choose the identity from a probe outcome and the descriptor's own
/// `st_dev`. A filesystem without a UUID (or a kernel without the call) falls
/// back to the device number; any other failure is an error.
pub(crate) fn select_filesystem_identity(
    probe: UuidProbe,
    st_dev: u64,
) -> Result<FilesystemIdentity> {
    match probe {
        UuidProbe::Reported { len, bytes } => {
            if len == 0 || usize::from(len) > MAX_UUID_BYTES {
                bail!("kernel reported a filesystem uuid of invalid length {len}");
            }
            let mut canonical = [0_u8; MAX_UUID_BYTES];
            canonical[..usize::from(len)].copy_from_slice(&bytes[..usize::from(len)]);
            Ok(FilesystemIdentity(Repr::Uuid {
                len,
                bytes: canonical,
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
        libc::ENOTTY | libc::EOPNOTSUPP | libc::ENOSYS | libc::EINVAL
    )
}

#[cfg(not(unix))]
fn uuid_unsupported(_errno: i32) -> bool {
    false
}

impl FilesystemIdentity {
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
            select_filesystem_identity(probe_uuid(file), st_dev)
        }
    }
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

    fn uuid(first: u8) -> UuidProbe {
        let mut bytes = [0; MAX_UUID_BYTES];
        bytes[0] = first;
        bytes[15] = 9;
        UuidProbe::Reported { len: 16, bytes }
    }

    #[test]
    fn same_uuid_with_different_device_is_the_same_filesystem() {
        let before = select_filesystem_identity(uuid(1), 100).unwrap();
        let after = select_filesystem_identity(uuid(1), 200).unwrap();
        assert_eq!(before, after);
    }

    #[test]
    fn different_uuid_is_a_different_filesystem() {
        let left = select_filesystem_identity(uuid(1), 100).unwrap();
        let right = select_filesystem_identity(uuid(2), 100).unwrap();
        assert_ne!(left, right);
    }

    #[test]
    fn missing_uuid_falls_back_to_the_device_number() {
        for errno in [libc::ENOTTY, libc::EOPNOTSUPP, libc::ENOSYS, libc::EINVAL] {
            let a = select_filesystem_identity(UuidProbe::Failed(errno), 7).unwrap();
            let b = select_filesystem_identity(UuidProbe::Failed(errno), 7).unwrap();
            let c = select_filesystem_identity(UuidProbe::Failed(errno), 8).unwrap();
            assert_eq!(a, b);
            assert_ne!(a, c);
        }
        assert_ne!(
            select_filesystem_identity(UuidProbe::Failed(libc::ENOTTY), 7).unwrap(),
            select_filesystem_identity(uuid(1), 7).unwrap()
        );
    }

    #[test]
    fn other_errors_and_invalid_lengths_are_refused() {
        assert!(select_filesystem_identity(UuidProbe::Failed(libc::EBADF), 7).is_err());
        assert!(select_filesystem_identity(UuidProbe::Failed(libc::EIO), 7).is_err());
        for len in [0_u8, 17] {
            let probe = UuidProbe::Reported {
                len,
                bytes: [1; MAX_UUID_BYTES],
            };
            assert!(select_filesystem_identity(probe, 7).is_err());
        }
    }

    #[test]
    fn bytes_beyond_the_reported_length_do_not_affect_equality() {
        let mut left = [0; MAX_UUID_BYTES];
        let mut right = [0; MAX_UUID_BYTES];
        left[0] = 5;
        right[0] = 5;
        right[10] = 77;
        let left = select_filesystem_identity(
            UuidProbe::Reported {
                len: 4,
                bytes: left,
            },
            1,
        );
        let right = select_filesystem_identity(
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
        let identity = select_filesystem_identity(uuid(0xab), 1).unwrap();
        let text = serde_json::to_string(&identity).unwrap();
        assert!(text.contains("ab"), "{text}");
        assert_eq!(
            serde_json::from_str::<FilesystemIdentity>(&text).unwrap(),
            identity
        );
        let device = select_filesystem_identity(UuidProbe::Failed(libc::ENOTTY), 42).unwrap();
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
