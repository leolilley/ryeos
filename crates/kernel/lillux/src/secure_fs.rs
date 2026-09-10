//! Descriptor-relative, no-follow reads and deterministic directory walks.
//!
//! These helpers are for authoritative inputs whose trust must not be rebound
//! by swapping a symlink or ancestor between a pathname check and open.

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use anyhow::{Context, Result};

/// Mechanical result of materializing one immutable file into a private
/// execution root. Policy remains with the caller; Lillux owns only the
/// descriptor-safe filesystem operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrivateFileMaterialization {
    Reflink,
    Copied,
}

fn try_reflink_regular_file(source: &File, target: &File) -> Result<bool> {
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd as _;

        // `_IOW(0x94, 9, int)` from linux/fs.h. Keep the syscall and its
        // platform vocabulary inside Lillux.
        const FICLONE: libc::c_ulong = 0x4004_9409;
        if unsafe { libc::ioctl(target.as_raw_fd(), FICLONE, source.as_raw_fd()) } == 0 {
            return Ok(true);
        }
        let error = std::io::Error::last_os_error();
        if matches!(
            error.raw_os_error(),
            Some(libc::EOPNOTSUPP) | Some(libc::ENOTTY) | Some(libc::EXDEV) | Some(libc::EINVAL)
        ) {
            return Ok(false);
        }
        Err(error).context("reflink immutable file into private directory")
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (source, target);
        Ok(false)
    }
}

/// Digest exactly the admitted number of bytes from an already-open regular
/// inode and prove its descriptor identity remained stable.
///
/// The size check happens before body I/O, so replacing an admitted file with
/// a huge or sparse file cannot make verification perform work beyond the
/// sealed file size. A one-byte sentinel still detects growth during the read.
pub fn digest_open_regular_file_stable_exact(
    file: &File,
    expected_bytes: u64,
) -> Result<(String, std::fs::Metadata)> {
    for attempt in 0..2 {
        let before = file.metadata()?;
        if before.len() != expected_bytes {
            anyhow::bail!(
                "regular file size {} differs from admitted size {expected_bytes}",
                before.len()
            );
        }
        let digest = digest_open_regular_file_exact(file, expected_bytes)?;
        let after = file.metadata()?;
        if after.len() == expected_bytes && same_regular_file_observation(&before, &after) {
            return Ok((digest, after));
        }
        if attempt == 1 {
            anyhow::bail!("regular file changed repeatedly while its content was being verified");
        }
    }
    unreachable!("bounded stable exact-digest loop always returns")
}

/// Normalize one descriptor-observed regular file to RyeOS's portable
/// project-snapshot mode contract. OS-specific permission inspection remains
/// inside Lillux; callers consume only the stable 0o644/0o755 result.
pub fn normalized_portable_regular_mode(metadata: &std::fs::Metadata) -> Result<u32> {
    #[cfg(not(unix))]
    {
        let _ = metadata;
        anyhow::bail!("portable regular-file mode inspection is unavailable on this platform")
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if !metadata.file_type().is_file() {
            anyhow::bail!("portable mode source is not a regular file");
        }
        Ok(if metadata.permissions().mode() & 0o111 == 0 {
            0o644
        } else {
            0o755
        })
    }
}

/// Match raw descriptor observations against one exact regular-file contract.
///
/// OS file-type bits and permission masks are Lillux vocabulary. Higher
/// layers supply the observed descriptor facts and their content-addressed
/// expectations without interpreting platform constants themselves.
pub fn matches_regular_file_identity(
    observed_size: u64,
    observed_mode: u32,
    observed_file_type: u32,
    expected_size: u64,
    expected_mode: u32,
) -> bool {
    #[cfg(not(unix))]
    {
        let _ = (
            observed_size,
            observed_mode,
            observed_file_type,
            expected_size,
            expected_mode,
        );
        false
    }
    #[cfg(unix)]
    {
        observed_size == expected_size
            && observed_file_type == libc::S_IFREG
            && observed_mode & 0o7777 == expected_mode
    }
}

fn same_regular_file_observation(before: &std::fs::Metadata, after: &std::fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        before.dev() == after.dev()
            && before.ino() == after.ino()
            && before.len() == after.len()
            && before.mode() == after.mode()
            && before.mtime() == after.mtime()
            && before.mtime_nsec() == after.mtime_nsec()
            && before.ctime() == after.ctime()
            && before.ctime_nsec() == after.ctime_nsec()
    }
    #[cfg(not(unix))]
    {
        before.len() == after.len()
            && before.permissions().readonly() == after.permissions().readonly()
            && before.modified().ok() == after.modified().ok()
    }
}

/// Opaque descriptor observation for a regular file. Platform identity and
/// timestamp fields stay inside Lillux; higher layers can bind a read to this
/// observation without interpreting OS metadata.
#[derive(Debug, Clone)]
pub struct OpenRegularFileObservation {
    metadata: std::fs::Metadata,
}

/// Stable platform identity and mutation-relevant metadata for one already-
/// open file. RyeOS may retain these values as an in-process fence, while all
/// raw metadata extraction and effective-user interpretation stay in Lillux.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpenFileIdentity {
    device: u64,
    inode: u64,
    size: u64,
    modified_seconds: i64,
    modified_nanoseconds: i64,
    changed_seconds: i64,
    changed_nanoseconds: i64,
    mode: u32,
    file_type: u32,
    owner: u32,
}

impl OpenFileIdentity {
    pub fn device(self) -> u64 {
        self.device
    }

    pub fn inode(self) -> u64 {
        self.inode
    }

    pub fn size(self) -> u64 {
        self.size
    }

    pub fn modified_seconds(self) -> i64 {
        self.modified_seconds
    }

    pub fn modified_nanoseconds(self) -> i64 {
        self.modified_nanoseconds
    }

    pub fn changed_seconds(self) -> i64 {
        self.changed_seconds
    }

    pub fn changed_nanoseconds(self) -> i64 {
        self.changed_nanoseconds
    }

    pub fn mode(self) -> u32 {
        self.mode
    }

    pub fn file_type(self) -> u32 {
        self.file_type
    }

    pub fn owner(self) -> u32 {
        self.owner
    }

    pub fn is_executable(self) -> bool {
        self.mode & 0o111 != 0
    }

    pub fn is_group_or_other_writable(self) -> bool {
        self.mode & 0o022 != 0
    }
}

/// Portable class of an already-open mount-source descriptor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenMountEntryKind {
    Regular,
    Directory,
    UnixSocket,
}

/// Serializable identity of an already-open directory. Platform coordinates
/// remain opaque to authoring and transaction layers; they may retain and
/// compare this value but never interpret its fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PinnedDirectoryIdentity {
    containing_device: u64,
    inode: u64,
}

#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RegularFileIdentity {
    containing_device: u64,
    inode: u64,
}

#[cfg(target_os = "linux")]
#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ConditionalByteReplacementRecovery {
    schema: u8,
    target_name: Vec<u8>,
    stage_name: Vec<u8>,
    quarantine_name: Vec<u8>,
    expected_target: RegularFileIdentity,
    staged_target: RegularFileIdentity,
}

#[cfg(target_os = "linux")]
fn regular_file_identity(file: &File) -> Result<RegularFileIdentity> {
    use std::os::unix::fs::MetadataExt as _;
    let metadata = file.metadata()?;
    if !metadata.file_type().is_file() {
        anyhow::bail!("descriptor is not a regular file");
    }
    Ok(RegularFileIdentity {
        containing_device: metadata.dev(),
        inode: metadata.ino(),
    })
}

impl OpenRegularFileObservation {
    pub fn size(&self) -> u64 {
        self.metadata.len()
    }

    pub fn portable_mode(&self) -> Result<u32> {
        normalized_portable_regular_mode(&self.metadata)
    }

    pub fn is_executable(&self) -> Result<bool> {
        #[cfg(not(unix))]
        anyhow::bail!("regular-file executable inspection is unavailable on this platform");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            Ok(self.metadata.permissions().mode() & 0o111 != 0)
        }
    }

    /// Preserve the incumbent regular file's permission bits when publishing
    /// an authored replacement. The platform representation remains private.
    pub fn permission_mode(&self) -> Result<u32> {
        #[cfg(not(unix))]
        anyhow::bail!("regular-file permission inspection is unavailable on this platform");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            if !self.metadata.file_type().is_file() {
                anyhow::bail!("permission source is not a regular file");
            }
            Ok(self.metadata.permissions().mode() & 0o777)
        }
    }

    /// Return the complete Unix permission class, including set-id/sticky
    /// bits, for policy code that must reject rather than preserve those
    /// special modes. Platform metadata interpretation remains in Lillux.
    pub fn full_permission_mode(&self) -> Result<u32> {
        #[cfg(not(unix))]
        anyhow::bail!("regular-file permission inspection is unavailable on this platform");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            if !self.metadata.file_type().is_file() {
                anyhow::bail!("permission source is not a regular file");
            }
            Ok(self.metadata.permissions().mode() & 0o7777)
        }
    }

    pub fn matches_directory_entry(&self, entry: &PinnedDirectoryEntryMetadata) -> bool {
        #[cfg(not(unix))]
        {
            let _ = entry;
            false
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;
            self.metadata.file_type().is_file()
                && entry.entry_type == PinnedEntryType::Regular
                && self.metadata.dev() == entry.containing_device
                && self.metadata.ino() == entry.inode
        }
    }

    /// Compare an incumbent observation after its exact directory entry was
    /// moved into Lillux quarantine. The namespace move necessarily changes
    /// ctime, so the comparison retains inode, size, permissions, and mtime;
    /// callers separately compare the complete bytes before publication.
    pub fn matches_quarantined_incumbent(&self, before: &Self) -> bool {
        #[cfg(not(unix))]
        {
            let _ = before;
            false
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;
            self.metadata.file_type().is_file()
                && before.metadata.file_type().is_file()
                && self.metadata.dev() == before.metadata.dev()
                && self.metadata.ino() == before.metadata.ino()
                && self.metadata.len() == before.metadata.len()
                && self.metadata.mode() == before.metadata.mode()
                && self.metadata.mtime() == before.metadata.mtime()
                && self.metadata.mtime_nsec() == before.metadata.mtime_nsec()
        }
    }
}

pub fn observe_open_regular_file(file: &File) -> Result<OpenRegularFileObservation> {
    let metadata = file.metadata()?;
    if !metadata.file_type().is_file() {
        anyhow::bail!("descriptor is not a regular file");
    }
    Ok(OpenRegularFileObservation { metadata })
}

/// Observe exact Unix file identity and mutation-relevant metadata without
/// exposing a metadata object for higher layers to reinterpret.
pub fn observe_open_file_identity(file: &File) -> Result<OpenFileIdentity> {
    #[cfg(not(unix))]
    {
        let _ = file;
        anyhow::bail!("open-file identity inspection is unavailable on this platform")
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;

        let metadata = file.metadata()?;
        Ok(OpenFileIdentity {
            device: metadata.dev(),
            inode: metadata.ino(),
            size: metadata.len(),
            modified_seconds: metadata.mtime(),
            modified_nanoseconds: metadata.mtime_nsec(),
            changed_seconds: metadata.ctime(),
            changed_nanoseconds: metadata.ctime_nsec(),
            mode: metadata.mode(),
            file_type: metadata.mode() & libc::S_IFMT,
            owner: metadata.uid(),
        })
    }
}

/// Require the exact open descriptor to remain a current-effective-user-owned
/// regular executable without group/other write authority.
pub fn require_effective_user_owned_executable(file: &File) -> Result<OpenFileIdentity> {
    #[cfg(not(unix))]
    {
        let _ = file;
        anyhow::bail!("owned executable inspection is unavailable on this platform")
    }
    #[cfg(unix)]
    {
        let identity = observe_open_file_identity(file)?;
        if identity.file_type != libc::S_IFREG
            || identity.owner != unsafe { libc::geteuid() }
            || !identity.is_executable()
            || identity.is_group_or_other_writable()
        {
            anyhow::bail!(
                "descriptor is not a current-user-owned executable regular file without group/other write bits (uid={}, mode={:#o})",
                identity.owner,
                identity.mode & 0o7777,
            );
        }
        Ok(identity)
    }
}

/// Require the exact open descriptor to remain a current-effective-user-owned
/// regular file without group/other write authority.
pub fn require_effective_user_owned_regular(file: &File) -> Result<OpenFileIdentity> {
    #[cfg(not(unix))]
    {
        let _ = file;
        anyhow::bail!("owned regular-file inspection is unavailable on this platform")
    }
    #[cfg(unix)]
    {
        let identity = observe_open_file_identity(file)?;
        if identity.file_type != libc::S_IFREG
            || identity.owner != unsafe { libc::geteuid() }
            || identity.is_group_or_other_writable()
        {
            anyhow::bail!(
                "descriptor is not a current-user-owned regular file without group/other write bits (uid={}, mode={:#o})",
                identity.owner,
                identity.mode & 0o7777,
            );
        }
        Ok(identity)
    }
}

/// Compare the inode identities of two already-open descriptors.
pub fn same_open_file_identity(left: &File, right: &File) -> Result<bool> {
    let left = observe_open_file_identity(left)?;
    let right = observe_open_file_identity(right)?;
    Ok(left.device == right.device && left.inode == right.inode)
}

/// Classify an already-open descriptor admitted as a mount source.
pub fn open_mount_entry_kind(file: &File) -> Result<OpenMountEntryKind> {
    let metadata = file.metadata()?;
    let file_type = metadata.file_type();
    if file_type.is_file() {
        return Ok(OpenMountEntryKind::Regular);
    }
    if file_type.is_dir() {
        return Ok(OpenMountEntryKind::Directory);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileTypeExt as _;
        if file_type.is_socket() {
            return Ok(OpenMountEntryKind::UnixSocket);
        }
    }
    anyhow::bail!("open descriptor is not a regular file, directory, or Unix socket")
}

pub fn ensure_open_regular_file_unchanged(
    file: &File,
    before: &OpenRegularFileObservation,
) -> Result<()> {
    let after = file.metadata()?;
    if !same_regular_file_observation(&before.metadata, &after) {
        anyhow::bail!("regular file changed while its content was being observed");
    }
    Ok(())
}

/// Read one already-open regular file through an exact descriptor observation.
/// The caller supplies the admitted byte ceiling; Lillux proves size, identity,
/// timestamps, and permissions stayed fixed for the whole read.
/// Positional reads are mandatory: cloned descriptors share a seek cursor and
/// must not corrupt one another's verification when reused concurrently.
pub fn read_open_regular_file_stable_bounded(
    file: &File,
    before: &OpenRegularFileObservation,
    max_bytes: u64,
) -> Result<Vec<u8>> {
    if before.size() > max_bytes {
        anyhow::bail!("regular file exceeds {max_bytes} bytes");
    }
    ensure_open_regular_file_unchanged(file, before)?;
    let expected = usize::try_from(before.size())
        .map_err(|_| anyhow::anyhow!("regular file size does not fit this platform"))?;
    let mut bytes = vec![0_u8; expected];
    let mut offset = 0;
    while offset < expected {
        let count = read_regular_file_at(file, &mut bytes[offset..], offset as u64)?;
        if count == 0 {
            anyhow::bail!("regular file ended before its observed length");
        }
        offset += count;
    }
    let mut sentinel = [0_u8; 1];
    if read_regular_file_at(file, &mut sentinel, before.size())? != 0 {
        anyhow::bail!("regular file grew while its content was being observed");
    }
    ensure_open_regular_file_unchanged(file, before)?;
    Ok(bytes)
}

fn read_regular_file_at(file: &File, bytes: &mut [u8], offset: u64) -> std::io::Result<usize> {
    loop {
        #[cfg(unix)]
        let result = std::os::unix::fs::FileExt::read_at(file, bytes, offset);
        #[cfg(windows)]
        let result = std::os::windows::fs::FileExt::seek_read(file, bytes, offset);
        #[cfg(not(any(unix, windows)))]
        let result = {
            let _ = (file, &bytes, offset);
            Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "positional regular-file reads are unsupported",
            ))
        };
        match result {
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            result => return result,
        }
    }
}

fn digest_open_regular_file_exact(file: &File, expected_bytes: u64) -> Result<String> {
    let mut digest = sha2::Sha256::new();
    use sha2::Digest as _;
    let mut buffer = [0_u8; 1024 * 1024];
    let mut remaining = expected_bytes;
    while remaining > 0 {
        let requested = usize::try_from(remaining.min(buffer.len() as u64))
            .expect("bounded digest chunk always fits usize");
        let read =
            read_regular_file_at(file, &mut buffer[..requested], expected_bytes - remaining)?;
        if read == 0 {
            anyhow::bail!("regular file ended before admitted size {expected_bytes} was consumed");
        }
        digest.update(&buffer[..read]);
        remaining -= read as u64;
    }
    let mut sentinel = [0_u8; 1];
    if read_regular_file_at(file, &mut sentinel, expected_bytes)? != 0 {
        anyhow::bail!("regular file grew beyond admitted size {expected_bytes}");
    }
    Ok(format!("{:x}", digest.finalize()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinnedEntryType {
    Directory,
    Regular,
    Symlink,
    CharacterDevice,
    BlockDevice,
    Fifo,
    Socket,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PinnedDirectoryEntryMetadata {
    pub name: OsString,
    pub entry_type: PinnedEntryType,
    pub mode: u32,
    /// Device-node identity (`st_rdev`). Meaningful only for device entries.
    pub device_id: u64,
    /// Containing-filesystem identity (`st_dev`). A traversal that must stay
    /// on one filesystem compares this against its pinned root, because a
    /// bind mount or separate filesystem below the root is neither bounded
    /// nor reproducible by the root's own declaration.
    pub containing_device: u64,
    /// Inode number, for identity comparisons within one filesystem.
    pub inode: u64,
}

#[cfg(unix)]
use std::os::fd::{AsRawFd, FromRawFd};
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;

#[cfg(unix)]
fn open_directory_no_follow(path: &Path) -> Result<Option<File>> {
    use std::path::Component;

    let start = if path.is_absolute() { "/" } else { "." };
    let start = std::ffi::CString::new(start).expect("static path contains no NUL");
    let descriptor = unsafe {
        libc::open(
            start.as_ptr(),
            libc::O_RDONLY
                | libc::O_DIRECTORY
                | libc::O_NOFOLLOW
                | libc::O_CLOEXEC
                | libc::O_NONBLOCK,
        )
    };
    if descriptor < 0 {
        return Err(std::io::Error::last_os_error()).context("open secure traversal root");
    }
    let mut directory = unsafe { File::from_raw_fd(descriptor) };
    for component in path.components() {
        let component = match component {
            Component::RootDir | Component::CurDir => continue,
            Component::Normal(component) => component,
            Component::ParentDir | Component::Prefix(_) => {
                anyhow::bail!(
                    "secure path contains an unsafe component: {}",
                    path.display()
                )
            }
        };
        let component = std::ffi::CString::new(component.as_bytes())?;
        let descriptor = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                component.as_ptr(),
                libc::O_RDONLY
                    | libc::O_DIRECTORY
                    | libc::O_NOFOLLOW
                    | libc::O_CLOEXEC
                    | libc::O_NONBLOCK,
            )
        };
        if descriptor < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::NotFound {
                return Ok(None);
            }
            return Err(error).with_context(|| format!("open secure directory {}", path.display()));
        }
        directory = unsafe { File::from_raw_fd(descriptor) };
    }
    Ok(Some(directory))
}

#[cfg(unix)]
fn open_or_create_directory_no_follow(path: &Path) -> Result<File> {
    use std::path::Component;

    let start = if path.is_absolute() { "/" } else { "." };
    let start = std::ffi::CString::new(start).expect("static path contains no NUL");
    let descriptor = unsafe {
        libc::open(
            start.as_ptr(),
            libc::O_RDONLY
                | libc::O_DIRECTORY
                | libc::O_NOFOLLOW
                | libc::O_CLOEXEC
                | libc::O_NONBLOCK,
        )
    };
    if descriptor < 0 {
        return Err(std::io::Error::last_os_error()).context("open secure traversal root");
    }
    let mut directory = unsafe { File::from_raw_fd(descriptor) };
    for component in path.components() {
        let component = match component {
            Component::RootDir | Component::CurDir => continue,
            Component::Normal(component) => component,
            Component::ParentDir | Component::Prefix(_) => {
                anyhow::bail!(
                    "secure path contains an unsafe component: {}",
                    path.display()
                )
            }
        };
        let component = std::ffi::CString::new(component.as_bytes())?;
        let mut descriptor = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                component.as_ptr(),
                libc::O_RDONLY
                    | libc::O_DIRECTORY
                    | libc::O_NOFOLLOW
                    | libc::O_CLOEXEC
                    | libc::O_NONBLOCK,
            )
        };
        if descriptor < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::NotFound {
                return Err(error)
                    .with_context(|| format!("open secure directory {}", path.display()));
            }
            if unsafe { libc::mkdirat(directory.as_raw_fd(), component.as_ptr(), 0o777) } != 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() != std::io::ErrorKind::AlreadyExists {
                    return Err(error)
                        .with_context(|| format!("create secure directory {}", path.display()));
                }
            }
            descriptor = unsafe {
                libc::openat(
                    directory.as_raw_fd(),
                    component.as_ptr(),
                    libc::O_RDONLY
                        | libc::O_DIRECTORY
                        | libc::O_NOFOLLOW
                        | libc::O_CLOEXEC
                        | libc::O_NONBLOCK,
                )
            };
            if descriptor < 0 {
                return Err(std::io::Error::last_os_error()).with_context(|| {
                    format!("open newly-created secure directory {}", path.display())
                });
            }
            directory
                .sync_all()
                .with_context(|| format!("sync secure directory parent {}", path.display()))?;
        }
        directory = unsafe { File::from_raw_fd(descriptor) };
    }
    Ok(directory)
}

#[cfg(unix)]
fn open_regular_at(
    parent: &File,
    name: &std::ffi::CStr,
    display_path: &Path,
) -> Result<Option<File>> {
    let descriptor = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
        )
    };
    if descriptor < 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::NotFound {
            return Ok(None);
        }
        return Err(error)
            .with_context(|| format!("open secure regular file {}", display_path.display()));
    }
    let file = unsafe { File::from_raw_fd(descriptor) };
    if !file.metadata()?.file_type().is_file() {
        anyhow::bail!(
            "secure input is not a regular file: {}",
            display_path.display()
        );
    }
    Ok(Some(file))
}

#[cfg(unix)]
fn open_child_directory(
    parent: &File,
    name: &std::ffi::CStr,
    display_path: &Path,
) -> Result<Option<File>> {
    let descriptor = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY
                | libc::O_DIRECTORY
                | libc::O_NOFOLLOW
                | libc::O_CLOEXEC
                | libc::O_NONBLOCK,
        )
    };
    if descriptor < 0 {
        let error = std::io::Error::last_os_error();
        if matches!(
            error.kind(),
            std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
        ) {
            return Ok(None);
        }
        return Err(error)
            .with_context(|| format!("open secure child directory {}", display_path.display()));
    }
    Ok(Some(unsafe { File::from_raw_fd(descriptor) }))
}

#[cfg(target_os = "linux")]
fn directory_names_bounded(
    directory: &File,
    max_entries: Option<usize>,
) -> Result<Vec<std::ffi::OsString>> {
    let fd_path = PathBuf::from(format!("/proc/self/fd/{}", directory.as_raw_fd()));
    let entries = std::fs::read_dir(&fd_path)
        .with_context(|| format!("enumerate pinned directory {}", fd_path.display()))?;
    let mut names = match max_entries {
        Some(max_entries) => entries
            .take(max_entries)
            .map(|entry| entry.map(|entry| entry.file_name()))
            .collect::<std::io::Result<Vec<_>>>()?,
        None => entries
            .map(|entry| entry.map(|entry| entry.file_name()))
            .collect::<std::io::Result<Vec<_>>>()?,
    };
    names.sort_by(|left, right| left.as_bytes().cmp(right.as_bytes()));
    Ok(names)
}

#[cfg(target_os = "linux")]
fn directory_names_with_limit(
    directory: &File,
    max_entries: usize,
) -> Result<Vec<std::ffi::OsString>> {
    let fd_path = PathBuf::from(format!("/proc/self/fd/{}", directory.as_raw_fd()));
    let entries = std::fs::read_dir(&fd_path)
        .with_context(|| format!("enumerate pinned directory {}", fd_path.display()))?;
    let read_limit = max_entries.saturating_add(1);
    let mut names = entries
        .take(read_limit)
        .map(|entry| entry.map(|entry| entry.file_name()))
        .collect::<std::io::Result<Vec<_>>>()?;
    if names.len() > max_entries {
        anyhow::bail!(
            "secure directory traversal exceeds maximum entry count {max_entries} at {}",
            fd_path.display()
        );
    }
    names.sort_by(|left, right| left.as_bytes().cmp(right.as_bytes()));
    Ok(names)
}

#[cfg(target_os = "linux")]
fn directory_names(directory: &File) -> Result<Vec<std::ffi::OsString>> {
    directory_names_bounded(directory, None)
}

/// A directory reached component-by-component with `O_NOFOLLOW`. Namespace
/// reads and mutations stay relative to this exact open directory inode.
#[derive(Debug)]
pub struct PinnedDirectory {
    path: PathBuf,
    directory: File,
}

/// One direct child opened without following links. Mixed-tree walkers use
/// this instead of probing a directory-only API and treating a regular file
/// as a structural error.
#[derive(Debug)]
pub enum PinnedDirectoryEntry {
    Directory(PinnedDirectory),
    Regular(File),
}

/// Exact authority for one regular child opened from a [`PinnedDirectory`].
///
/// The descriptor, its direct-child coordinate, and its diagnostic pathname
/// form one inseparable authority object. The pathname is never sufficient
/// authority to reopen or mutate the entry, and the child name must not be
/// paired with an independently obtained descriptor. Callers therefore use
/// the typed read, observation, conversion, and directory-mutation methods
/// below instead of extracting and recombining the underlying fields.
#[derive(Debug)]
pub struct PinnedRegularFile {
    path: PathBuf,
    name: OsString,
    file: File,
}

/// Verify an absolute spelling inside an already-confined mount namespace.
/// Read-only mounts prevent entry replacement; the caller's confinement must
/// additionally prevent subsequent mount changes. This does not establish
/// immutability of bytes through aliases outside that namespace.
fn verified_read_only_namespace_path(path: &Path, held: &File) -> Result<PathBuf> {
    verified_namespace_path(path, held, NamespaceLeafAccess::ReadOnly)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum NamespaceLeafAccess {
    ReadOnly,
    WritableDirectory,
}

fn verified_namespace_path(
    path: &Path,
    held: &File,
    leaf_access: NamespaceLeafAccess,
) -> Result<PathBuf> {
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (path, held, leaf_access);
        anyhow::bail!("verified namespace paths are unavailable on this platform");
    }
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::ffi::OsStrExt as _;
        use std::path::Component;

        if !path.is_absolute() || path.as_os_str().as_bytes().len() >= libc::PATH_MAX as usize {
            anyhow::bail!("namespace path must be bounded, absolute and normalized");
        }
        let normalized = path.components().collect::<PathBuf>();
        if normalized.as_os_str().as_bytes() != path.as_os_str().as_bytes()
            || path
                .components()
                .any(|part| !matches!(part, Component::RootDir | Component::Normal(_)))
        {
            anyhow::bail!("namespace path must be bounded, absolute and normalized");
        }
        let held_metadata = held.metadata()?;
        if leaf_access == NamespaceLeafAccess::WritableDirectory
            && (!held_metadata.is_dir() || path == Path::new("/"))
        {
            anyhow::bail!("writable namespace leaf must be a non-root pinned directory");
        }
        let mut directory = PinnedDirectory::open(Path::new("/"))?
            .ok_or_else(|| anyhow::anyhow!("namespace root disappeared"))?;
        require_namespace_mount_access(&directory.directory, NamespaceLeafAccess::ReadOnly)?;
        let mut components = path.components().skip(1).peekable();
        while let Some(Component::Normal(name)) = components.next() {
            if components.peek().is_none() && held_metadata.is_file() {
                let current = directory
                    .open_pinned_regular(name, false)?
                    .ok_or_else(|| anyhow::anyhow!("namespace file disappeared"))?;
                require_namespace_mount_access(&current.file, NamespaceLeafAccess::ReadOnly)?;
                require_same_namespace_object(held, &current.file)?;
                return Ok(path.to_path_buf());
            }
            directory = directory
                .open_child_directory(name)?
                .ok_or_else(|| anyhow::anyhow!("namespace directory disappeared"))?;
            require_namespace_mount_access(
                &directory.directory,
                if components.peek().is_none() {
                    leaf_access
                } else {
                    NamespaceLeafAccess::ReadOnly
                },
            )?;
        }
        require_same_namespace_object(held, &directory.directory)?;
        Ok(path.to_path_buf())
    }
}

#[cfg(target_os = "linux")]
fn require_namespace_mount_access(file: &File, access: NamespaceLeafAccess) -> Result<()> {
    let mut stats = std::mem::MaybeUninit::<libc::statvfs>::zeroed();
    if unsafe { libc::fstatvfs(file.as_raw_fd(), stats.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error()).context("inspect namespace mount flags");
    }
    let read_only = unsafe { stats.assume_init() }.f_flag & libc::ST_RDONLY != 0;
    match access {
        NamespaceLeafAccess::ReadOnly if !read_only => {
            anyhow::bail!("namespace path traverses a writable mount");
        }
        NamespaceLeafAccess::WritableDirectory if read_only => {
            anyhow::bail!("namespace directory leaf is not mounted writable");
        }
        _ => {}
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn require_same_namespace_object(held: &File, current: &File) -> Result<()> {
    use std::os::unix::fs::MetadataExt as _;
    let held = held.metadata()?;
    let current = current.metadata()?;
    if held.dev() != current.dev()
        || held.ino() != current.ino()
        || held.mode() & libc::S_IFMT != current.mode() & libc::S_IFMT
        || !(held.is_file() || held.is_dir())
    {
        anyhow::bail!("namespace path does not select the exact pinned object");
    }
    Ok(())
}

/// One process-scoped generation in a strict flat regular-file namespace.
///
/// Creation, stale-generation collection, advisory lock ownership, host-clock
/// naming, process coordinates, and exact descriptor-relative teardown stay
/// inside Lillux. Higher layers retain only the generation directory and put
/// their own semantic contents inside it.
#[derive(Debug)]
pub struct ProcessScopedFlatDirectoryGeneration {
    generations: PinnedDirectory,
    generation_name: OsString,
    directory: PinnedDirectory,
    _lifetime_lock: PinnedRegularFile,
}

/// Opaque wall-clock ordering coordinate for one descriptor-observed
/// filesystem modification time.
///
/// Callers may compare these values to choose an operational eviction order,
/// but cannot reinterpret the host clock as durable authority. Reading and
/// converting the platform timestamp remains inside Lillux.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct FilesystemModificationTime(SystemTime);

impl ProcessScopedFlatDirectoryGeneration {
    /// Create a new generation below a descriptor-rooted relative namespace.
    /// Every component is owner-private and no symlink is followed.
    pub fn create_relative(root: &PinnedDirectory, components: &[&str]) -> Result<Self> {
        #[cfg(not(unix))]
        {
            let _ = (root, components);
            anyhow::bail!("process-scoped directory generations require Unix file locking");
        }
        #[cfg(unix)]
        {
            use std::sync::atomic::{AtomicU64, Ordering};

            const MAX_NAMESPACE_COMPONENTS: usize = 32;
            const MAX_GENERATIONS: usize = 4_096;
            if components.is_empty() || components.len() > MAX_NAMESPACE_COMPONENTS {
                anyhow::bail!(
                    "process-scoped generation namespace must contain 1..={MAX_NAMESPACE_COMPONENTS} components"
                );
            }
            static NEXT_GENERATION: AtomicU64 = AtomicU64::new(0);
            let mut generations = root.try_clone()?;
            for component in components {
                generations = generations.open_or_create_child(OsStr::new(*component), 0o700)?;
            }
            generations.set_mode(0o700)?;

            // This file serializes generation publication with stale cleanup
            // across processes. The pinned file, not its pathname, retains the
            // advisory lock for this scope.
            let cleanup_lock = generations.open_pinned_regular_create(
                OsStr::new(".cleanup.lock"),
                true,
                false,
                0o600,
            )?;
            cleanup_lock.set_mode(0o600)?;
            cleanup_lock.lock_exclusive()?;

            let created_at = crate::time::timestamp_millis();
            let mut created = None;
            for _ in 0..1_024 {
                let sequence = NEXT_GENERATION.fetch_add(1, Ordering::Relaxed);
                let name =
                    OsString::from(format!("{}-{created_at}-{sequence}", std::process::id()));
                match generations.create_child(&name, 0o700) {
                    Ok(directory) => {
                        created = Some((name, directory));
                        break;
                    }
                    Err(error)
                        if error.downcast_ref::<std::io::Error>().is_some_and(|error| {
                            error.kind() == std::io::ErrorKind::AlreadyExists
                        }) => {}
                    Err(error) => return Err(error),
                }
            }
            let (generation_name, directory) = created.ok_or_else(|| {
                anyhow::anyhow!("could not reserve a unique directory generation")
            })?;
            let setup = (|| -> Result<PinnedRegularFile> {
                directory.set_mode(0o700)?;
                let lifetime_lock = directory.open_pinned_regular_create(
                    OsStr::new(".lifetime.lock"),
                    true,
                    true,
                    0o600,
                )?;
                lifetime_lock.set_mode(0o600)?;
                if !lifetime_lock.try_lock_exclusive()? {
                    anyhow::bail!("new directory-generation lifetime lock was already held");
                }

                let names = generations.entry_names_bounded(MAX_GENERATIONS + 1)?;
                if names.len() > MAX_GENERATIONS {
                    anyhow::bail!("flat generation namespace exceeds {MAX_GENERATIONS} entries");
                }
                for name in names {
                    if name == generation_name || name.as_os_str() == OsStr::new(".cleanup.lock") {
                        continue;
                    }
                    let Some(stale) = generations.open_child_directory(&name)? else {
                        anyhow::bail!(
                            "flat generation namespace contains unsupported entry {}",
                            generations.path().join(&name).display()
                        );
                    };
                    let _stale_lifetime_lock =
                        match stale.open_pinned_regular(OsStr::new(".lifetime.lock"), true)? {
                            Some(lock) if lock.try_lock_exclusive()? => Some(lock),
                            Some(_) => continue,
                            None => None,
                        };
                    remove_flat_directory_generation(&generations, &name, &stale)?;
                }
                Ok(lifetime_lock)
            })();
            let lifetime_lock = match setup {
                Ok(lock) => lock,
                Err(error) => {
                    return match remove_flat_directory_generation(
                        &generations,
                        &generation_name,
                        &directory,
                    ) {
                        Ok(()) => Err(error),
                        Err(cleanup) => Err(error.context(format!(
                            "new directory-generation cleanup also failed: {cleanup:#}"
                        ))),
                    };
                }
            };

            Ok(Self {
                generations,
                generation_name,
                directory,
                _lifetime_lock: lifetime_lock,
            })
        }
    }

    /// Exact newly-created generation directory retained for this process.
    pub fn directory(&self) -> &PinnedDirectory {
        &self.directory
    }
}

impl Drop for ProcessScopedFlatDirectoryGeneration {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Ok(Some(cleanup_lock)) = self
            .generations
            .open_pinned_regular(OsStr::new(".cleanup.lock"), true)
            && cleanup_lock.lock_exclusive().is_ok()
        {
            let _ = remove_flat_directory_generation(
                &self.generations,
                &self.generation_name,
                &self.directory,
            );
        }
    }
}

fn remove_flat_directory_generation(
    generations: &PinnedDirectory,
    name: &OsStr,
    directory: &PinnedDirectory,
) -> Result<()> {
    for entry in directory.regular_files()? {
        directory.remove_pinned_regular_if_same(&entry)?;
    }
    if !generations.remove_empty_child_if_same(name, directory)? {
        anyhow::bail!(
            "directory generation is not a flat regular-file namespace: {}",
            directory.path().display()
        );
    }
    Ok(())
}

impl PinnedRegularFile {
    /// Require administrator-selected ownership of this exact file. Callers
    /// must separately protect its containing namespace; mode bits on a file
    /// alone cannot prevent replacement through a writable parent.
    pub fn require_owner(&self, owner: u32) -> Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;
            let metadata = self.file.metadata()?;
            if metadata.uid() != owner || metadata.mode() & 0o022 != 0 || metadata.nlink() != 1 {
                anyhow::bail!(
                    "pinned file has unsafe ownership, permissions or links: {}",
                    self.path.display()
                );
            }
            Ok(())
        }
        #[cfg(not(unix))]
        {
            let _ = owner;
            anyhow::bail!("owned host files are unavailable on this platform")
        }
    }

    /// Return the pathname recorded when this authority was opened.
    ///
    /// This is diagnostic context only. It does not prove that the namespace
    /// still selects this inode and must never be used to reopen the file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Return the direct-child coordinate retained with this authority.
    ///
    /// The name is useful for diagnostics and policy matching. Namespace
    /// mutations should accept the complete [`PinnedRegularFile`] so this
    /// coordinate cannot accidentally be paired with another descriptor.
    pub fn name(&self) -> &OsStr {
        &self.name
    }

    /// Read this exact already-open regular file under a hard byte ceiling.
    pub fn read_bounded(&self, max_bytes: u64) -> Result<Vec<u8>> {
        read_open_regular_file_bounded(self.file.try_clone()?, max_bytes)
            .with_context(|| format!("read pinned regular file {}", self.path.display()))
    }

    /// Observe the exact descriptor for a later stable conditional read.
    pub fn observation(&self) -> Result<OpenRegularFileObservation> {
        observe_open_regular_file(&self.file)
    }

    /// Return the age of this exact file inode's modification time. This is a
    /// diagnostic clock observation, never file identity or stale authority.
    pub fn modification_age(&self) -> Result<Option<crate::time::Duration>> {
        let metadata = self
            .file
            .metadata()
            .with_context(|| format!("inspect pinned regular file {}", self.path.display()))?;
        Ok(metadata
            .modified()
            .ok()
            .and_then(|modified| SystemTime::now().duration_since(modified).ok()))
    }

    /// Read this exact descriptor and prove it still matches a prior
    /// observation. The cloned raw descriptor remains internal to Lillux.
    pub fn read_stable_bounded(
        &self,
        observation: &OpenRegularFileObservation,
        max_bytes: u64,
    ) -> Result<Vec<u8>> {
        let mut file = self.file.try_clone()?;
        read_open_regular_file_stable_bounded(&mut file, observation, max_bytes)
    }

    /// Digest this exact descriptor at the size committed by `observation`
    /// and prove that its identity and metadata remained stable throughout.
    /// The descriptor clone used by the digest implementation does not escape
    /// this typed authority boundary.
    pub fn digest_stable_exact(&self, observation: &OpenRegularFileObservation) -> Result<String> {
        let mut file = self.file.try_clone()?;
        let (digest, after) = digest_open_regular_file_stable_exact(&mut file, observation.size())?;
        if !same_regular_file_observation(&observation.metadata, &after) {
            anyhow::bail!(
                "pinned regular file changed while it was being digested: {}",
                self.path.display()
            );
        }
        Ok(digest)
    }

    /// Return the ordinary permission bits of this exact pinned inode.
    pub fn permission_mode(&self) -> Result<u32> {
        observe_open_regular_file(&self.file)?.permission_mode()
    }

    /// Apply an exact portable mode to this pinned regular inode.
    pub fn set_mode(&self, mode: u32) -> Result<()> {
        set_open_regular_file_mode(&self.file, mode)
            .with_context(|| format!("protect pinned regular file {}", self.path.display()))
    }

    /// Return the byte length observed from this exact descriptor.
    pub fn size(&self) -> Result<u64> {
        let metadata = self
            .file
            .metadata()
            .with_context(|| format!("inspect pinned regular file {}", self.path.display()))?;
        if !metadata.file_type().is_file() {
            anyhow::bail!("pinned descriptor is not a regular file");
        }
        Ok(metadata.len())
    }

    /// Acquire an exclusive advisory lock on this exact open inode.
    ///
    /// The lock is attached to the retained descriptor, never to a pathname
    /// reopened by the caller. Dropping this authority (and all descriptor
    /// clones made from it) releases the process's ownership as defined by the
    /// platform lock contract.
    pub fn lock_exclusive(&self) -> Result<()> {
        #[cfg(not(unix))]
        anyhow::bail!("regular-file advisory locking is unavailable on this platform");
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd as _;
            if unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_EX) } != 0 {
                return Err(std::io::Error::last_os_error())
                    .with_context(|| format!("lock pinned regular file {}", self.path.display()));
            }
            Ok(())
        }
    }

    /// Acquire a shared advisory lock on this exact open inode.
    ///
    /// The retained [`PinnedRegularFile`] is the lock authority. Dropping it
    /// (and every descriptor clone derived from it) releases the shared lease
    /// according to the platform lock contract.
    pub fn lock_shared(&self) -> Result<()> {
        #[cfg(not(unix))]
        anyhow::bail!("regular-file advisory locking is unavailable on this platform");
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd as _;
            if unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_SH) } != 0 {
                return Err(std::io::Error::last_os_error()).with_context(|| {
                    format!("shared-lock pinned regular file {}", self.path.display())
                });
            }
            Ok(())
        }
    }

    /// Attempt to acquire an exclusive advisory lock on this exact inode.
    /// Returns `false` only when another owner currently holds the lock.
    pub fn try_lock_exclusive(&self) -> Result<bool> {
        #[cfg(not(unix))]
        anyhow::bail!("regular-file advisory locking is unavailable on this platform");
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd as _;
            if unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                return Ok(true);
            }
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::WouldBlock {
                Ok(false)
            } else {
                Err(error).with_context(|| {
                    format!("try-lock pinned regular file {}", self.path.display())
                })
            }
        }
    }

    /// Refresh the modification time of this exact open inode to the host's
    /// current wall clock. This is an operational recency signal only; it must
    /// never participate in durable identity or authorization.
    pub fn refresh_modification_time(&self) -> Result<()> {
        self.file
            .set_modified(SystemTime::now())
            .with_context(|| format!("touch pinned regular file {}", self.path.display()))
    }

    /// Observe this exact inode's modification time as an opaque ordering
    /// coordinate. Filesystems that cannot report a modification time return
    /// `None`; callers can conservatively treat that as oldest.
    pub fn modification_time(&self) -> Result<Option<FilesystemModificationTime>> {
        let metadata = self
            .file
            .metadata()
            .with_context(|| format!("inspect pinned regular file {}", self.path.display()))?;
        Ok(metadata.modified().ok().map(FilesystemModificationTime))
    }

    /// Duplicate this exact open descriptor for a boundary whose own type
    /// retains descriptor authority (for example CAS ingestion or isolation).
    ///
    /// This is not a general namespace escape hatch: callers must not combine
    /// the returned descriptor with [`Self::name`] or [`Self::path`] to perform
    /// pathname operations. Such operations belong on `PinnedDirectory` and
    /// take the complete `PinnedRegularFile` authority instead.
    pub fn try_clone_descriptor(&self) -> Result<File> {
        self.file
            .try_clone()
            .with_context(|| format!("duplicate pinned regular file {}", self.path.display()))
    }

    /// Derive a child descriptor with one registered lifetime owner. The
    /// original remains a pinned byte source, never a plain inherited handle.
    pub fn inherited_descriptor_authority(
        &self,
    ) -> Result<crate::exec::InheritedDescriptorAuthority> {
        #[cfg(not(unix))]
        anyhow::bail!("inherited descriptor authority is unavailable on this platform");
        #[cfg(unix)]
        {
            let lease = crate::exec::retain_fork_sensitive_descriptors();
            let file = self.file.try_clone()?;
            crate::exec::InheritedDescriptorAuthority::from_owned_file(file, &lease)
                .map_err(anyhow::Error::msg)
        }
    }

    /// Require this already-open regular file to carry at least one executable
    /// mode bit. The check remains descriptor-relative and OS mechanics stay
    /// inside Lillux.
    pub fn require_executable(&self) -> Result<()> {
        let metadata = self
            .file
            .metadata()
            .with_context(|| format!("inspect pinned regular file {}", self.path.display()))?;
        if !metadata.is_file() {
            anyhow::bail!(
                "pinned executable is not a regular file: {}",
                self.path.display()
            );
        }
        #[cfg(not(unix))]
        anyhow::bail!("descriptor-relative executable-mode checks are unavailable");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            if metadata.permissions().mode() & 0o111 == 0 {
                anyhow::bail!(
                    "pinned regular file has no executable bit: {}",
                    self.path.display()
                );
            }
            Ok(())
        }
    }

    /// Consume this exact file authority into a descriptor-rooted child path.
    ///
    /// For ordinary descendant processes in a sealed namespace, see
    /// [`Self::verified_read_only_namespace_path`].
    pub fn into_inherited_descriptor_path(
        self,
    ) -> Result<crate::exec::InheritedDescriptorAuthority> {
        crate::exec::inherited_descriptor_path(self.file).map_err(anyhow::Error::msg)
    }

    /// Prove that this absolute, no-follow spelling still selects this exact
    /// file and traverses only read-only mounts, including every ancestor.
    /// Unlike descriptor paths it survives ordinary descendants closing FDs.
    /// The caller must already confine mount changes (as Lillux's native
    /// sandbox does); this check does not seal mounts or immutable bytes.
    pub fn verified_read_only_namespace_path(&self) -> Result<PathBuf> {
        verified_read_only_namespace_path(&self.path, &self.file)
    }
}

/// Sequential reads remain bound to the exact inode retained by this typed
/// authority. Implementing `Read` and `Seek` avoids forcing archive and parser
/// consumers to extract a raw descriptor merely to consume bytes.
impl std::io::Read for PinnedRegularFile {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        self.file.read(buffer)
    }
}

impl std::io::Seek for PinnedRegularFile {
    fn seek(&mut self, position: std::io::SeekFrom) -> std::io::Result<u64> {
        self.file.seek(position)
    }
}

/// Hard limits for one descriptor-relative directory traversal.
///
/// `max_entries` counts every observed child name, including directories and
/// entries later pruned by the caller. `max_depth` counts descendant directory
/// components below the opened root. The budget is enforced before a
/// directory's names are retained or a child directory is entered, so caller
/// callbacks are not the resource boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DirectoryTraversalBudget {
    pub max_entries: usize,
    pub max_depth: usize,
}

#[cfg(unix)]
struct DirectoryTraversalState {
    remaining_entries: usize,
    max_depth: usize,
}

impl DirectoryTraversalBudget {
    pub const fn new(max_entries: usize, max_depth: usize) -> Self {
        Self {
            max_entries,
            max_depth,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OwnerPrivateTreeAction {
    Require,
    RequireEnclosed,
    Tighten,
}

/// Apply an exact portable mode to one already-opened regular-file inode.
///
/// Authority-sensitive callers use this after create because the process
/// umask may narrow the creation mode. The descriptor, rather than a pathname,
/// remains the mutation authority throughout.
pub fn set_open_regular_file_mode(file: &File, mode: u32) -> Result<()> {
    #[cfg(not(unix))]
    {
        let _ = (file, mode);
        anyhow::bail!("descriptor-relative regular-file permissions are unavailable")
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;

        if mode & !0o777 != 0 {
            anyhow::bail!("regular-file mode contains non-portable bits: {mode:#o}");
        }
        let before = file.metadata()?;
        if !before.file_type().is_file() {
            anyhow::bail!("permission target is not a regular file");
        }
        if unsafe { libc::fchmod(file.as_raw_fd(), mode as libc::mode_t) } != 0 {
            return Err(std::io::Error::last_os_error())
                .context("set descriptor-relative regular-file permissions");
        }
        let after = file.metadata()?;
        if !after.file_type().is_file() || after.mode() & 0o7777 != mode {
            anyhow::bail!(
                "descriptor-relative regular-file mode is {:#o}, expected {mode:#o}",
                after.mode() & 0o7777
            );
        }
        Ok(())
    }
}

/// RAII advisory lock for a pinned directory inode. Directory-scoped writers
/// use this without introducing lock-anchor files into closed namespaces.
#[derive(Debug, Clone)]
pub struct PinnedDirectoryLock {
    inner: Arc<PinnedDirectoryLockInner>,
}

#[derive(Debug)]
struct PinnedDirectoryLockInner {
    file: File,
}

/// Complete hidden file awaiting a batch durability barrier and create-only
/// publication. Dropping an unpublished value removes its temporary name.
pub(crate) struct PreparedAtomicCreate {
    directory: PinnedDirectory,
    temp_name: std::ffi::CString,
    target_name: std::ffi::CString,
    target_path: PathBuf,
    _temp_file: File,
    published: bool,
}

impl PreparedAtomicCreate {
    /// Publish the already-written hidden file without replacing authority.
    /// `false` means another writer won the target name; the caller must verify
    /// that winner's exact bytes.
    pub(crate) fn publish(mut self) -> Result<bool> {
        #[cfg(not(unix))]
        anyhow::bail!("secure prepared publication is unavailable on this platform");
        #[cfg(unix)]
        {
            match publish_temp_without_replacement(
                &self.directory.directory,
                &self.temp_name,
                &self.target_name,
                &self.target_path,
            ) {
                Ok(()) => {
                    self.published = true;
                    Ok(true)
                }
                Err(error) => {
                    if self
                        .directory
                        .open_regular(
                            self.target_path.file_name().ok_or_else(|| {
                                anyhow::anyhow!("prepared target has no filename")
                            })?,
                            false,
                        )?
                        .is_some()
                    {
                        Ok(false)
                    } else {
                        Err(error)
                    }
                }
            }
        }
    }
}

impl Drop for PreparedAtomicCreate {
    fn drop(&mut self) {
        #[cfg(unix)]
        if !self.published {
            unsafe {
                libc::unlinkat(
                    self.directory.directory.as_raw_fd(),
                    self.temp_name.as_ptr(),
                    0,
                );
            }
        }
    }
}

impl Drop for PinnedDirectoryLockInner {
    fn drop(&mut self) {
        #[cfg(unix)]
        unsafe {
            libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

impl PinnedDirectoryLock {
    /// Replace this process while retaining the SAME locked open description.
    /// Success never runs this guard's unlock destructor; failure keeps normal
    /// RAII cleanup. Do not implement this as spawn + dropping the guard: an
    /// explicit LOCK_UN would release the child's duplicated lock too.
    /// `locator_env` carries only a descriptor locator. The receiver must call
    /// require_inherited_exclusive_lock against its independently selected root.
    pub fn exec_command_with_lock(
        &self,
        command: &mut std::process::Command,
        locator_env: &str,
    ) -> Result<std::convert::Infallible> {
        if locator_env.is_empty() || locator_env.contains(['=', '\0']) {
            anyhow::bail!("invalid inherited lock locator name");
        }
        #[cfg(unix)]
        {
            let lease = crate::exec::retain_fork_sensitive_descriptors();
            let inherited = crate::exec::InheritedDescriptorAuthority::from_owned_file(
                self.inner.file.try_clone()?,
                &lease,
            )
            .map_err(anyhow::Error::msg)?;
            drop(lease);
            command.env(
                locator_env,
                inherited
                    .inherited_descriptor()
                    .map_err(anyhow::Error::msg)?
                    .to_string(),
            );
            crate::configure_inherited_descriptor_authorities(
                command,
                std::slice::from_ref(&inherited),
            )
            .map_err(anyhow::Error::msg)?;
            Err(crate::replace_current_process(command))
                .context("exec while retaining directory transaction lock")
        }
        #[cfg(not(unix))]
        {
            let _ = command;
            anyhow::bail!("inherited directory transaction locks are unavailable on this OS")
        }
    }

    /// Prove that this guard protects the exact directory inode selected by
    /// `directory`. Cloned guards share one underlying flock and release it
    /// only after the last guard is dropped.
    pub fn ensure_protects(&self, directory: &PinnedDirectory) -> Result<()> {
        #[cfg(not(unix))]
        {
            let _ = directory;
            anyhow::bail!("pinned directory lock identity is unavailable on this platform");
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;

            let locked = self.inner.file.metadata()?;
            let selected = directory.directory.metadata()?;
            if locked.dev() != selected.dev() || locked.ino() != selected.ino() {
                anyhow::bail!(
                    "pinned directory lock does not protect {}",
                    directory.path.display()
                );
            }
            Ok(())
        }
    }
}

impl PinnedDirectory {
    /// Validate a transported flock without unlocking it on return. Identity
    /// alone is insufficient: an independent open must contend and the supplied
    /// open description must itself own exclusive access. A numeric descriptor
    /// or environment flag alone grants nothing. The caller retains the original
    /// inherited descriptor for its complete transaction lifetime.
    pub fn require_inherited_exclusive_lock(&self, descriptor: u32) -> Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;
            let descriptor =
                i32::try_from(descriptor).context("inherited lock descriptor is out of range")?;
            if descriptor <= libc::STDERR_FILENO {
                anyhow::bail!("inherited lock overlaps stdio");
            }
            let duplicate = unsafe { libc::fcntl(descriptor, libc::F_DUPFD_CLOEXEC, 3) };
            if duplicate < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            let candidate = unsafe { File::from_raw_fd(duplicate) };
            let held = candidate.metadata()?;
            let expected = self.directory.metadata()?;
            if !held.is_dir() || held.dev() != expected.dev() || held.ino() != expected.ino() {
                anyhow::bail!("inherited transaction lock names a different directory");
            }
            let probe = self.independent_lock_descriptor()?;
            if unsafe { libc::flock(probe.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                // Closing this independently-open probe releases only its own
                // acquired lock. No candidate lock was held in this case.
                anyhow::bail!("selected directory has no inherited transaction owner");
            }
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::EWOULDBLOCK) {
                return Err(error).context("probe inherited transaction exclusion");
            }
            if unsafe { libc::flock(candidate.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
                return Err(std::io::Error::last_os_error())
                    .context("supplied descriptor does not own the transaction lock");
            }
            // File drops close duplicates only. Never LOCK_UN this shared OFD.
            Ok(())
        }
        #[cfg(not(unix))]
        {
            let _ = descriptor;
            anyhow::bail!("inherited directory transaction locks are unavailable on this OS")
        }
    }

    /// Select this exact directory for a child/controller exec. Do not turn a
    /// verified pin back into Command::current_dir(path): a mutable parent can
    /// replace that spelling between admission and exec.
    pub fn configure_command_cwd(&self, command: &mut std::process::Command) -> Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt as _;
            let directory = self.directory.try_clone()?;
            // SAFETY: the hook owns the descriptor through exec; fchdir is
            // async-signal-safe and affects only the launching process.
            unsafe {
                command.pre_exec(move || {
                    if libc::fchdir(directory.as_raw_fd()) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
            Ok(())
        }
        #[cfg(not(unix))]
        {
            let _ = command;
            anyhow::bail!("descriptor-selected working directories are unavailable")
        }
    }

    /// Check this exact directory, not a pathname reopened after validation.
    pub fn require_owner(&self, owner: u32) -> Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;
            let metadata = self.directory.metadata()?;
            if metadata.uid() != owner || metadata.mode() & 0o022 != 0 {
                anyhow::bail!(
                    "pinned directory has unsafe ownership or permissions: {}",
                    self.path.display()
                );
            }
            Ok(())
        }
        #[cfg(not(unix))]
        {
            let _ = owner;
            anyhow::bail!("owned host directories are unavailable on this platform")
        }
    }

    /// Open an absolute host-configuration path only through directories owned
    /// by the selected administrator and not writable by others. Missing is
    /// distinct from an unsafe ancestor/symlink and never conceals that error.
    pub fn open_owned_hierarchy(path: &Path, owner: u32) -> Result<Option<Self>> {
        if !path.is_absolute() {
            anyhow::bail!("owned host hierarchy requires an absolute path");
        }
        let mut directory = Self::open(Path::new("/"))?
            .ok_or_else(|| anyhow::anyhow!("host filesystem root is unavailable"))?;
        directory.require_owner(owner)?;
        for component in path.components() {
            match component {
                std::path::Component::RootDir => continue,
                std::path::Component::Normal(name) => {
                    let Some(child) = directory.open_child_directory(name)? else {
                        return Ok(None);
                    };
                    child.require_owner(owner)?;
                    directory = child;
                }
                _ => anyhow::bail!("owned host hierarchy has a noncanonical component"),
            }
        }
        Ok(Some(directory))
    }

    /// Adopt an already-open directory descriptor as a descriptor-relative
    /// authority. `path` is diagnostic only; all subsequent traversal and
    /// mutation remains rooted in `directory`.
    pub fn from_open_directory(path: PathBuf, directory: File) -> Result<Self> {
        if !directory.metadata()?.is_dir() {
            anyhow::bail!("open authority is not a directory: {}", path.display());
        }
        Ok(Self { path, directory })
    }

    pub fn identity(&self) -> Result<PinnedDirectoryIdentity> {
        let (containing_device, inode) = self.device_inode()?;
        Ok(PinnedDirectoryIdentity {
            containing_device,
            inode,
        })
    }

    /// Return the age of this exact directory inode's modification time.
    ///
    /// Wall-clock observation and subtraction stay inside Lillux. `None`
    /// means the filesystem does not expose a usable timestamp or reports a
    /// value in the future, so callers must not classify it as stale.
    pub fn modification_age(&self) -> Result<Option<crate::time::Duration>> {
        let metadata = self
            .directory
            .metadata()
            .with_context(|| format!("inspect pinned directory {}", self.path.display()))?;
        Ok(metadata
            .modified()
            .ok()
            .and_then(|modified| SystemTime::now().duration_since(modified).ok()))
    }

    /// Remove every entry below this exact pinned directory without following
    /// symlinks or crossing a mounted filesystem boundary. The directory
    /// itself remains open and is not removed.
    pub fn remove_contents_recursive(&self) -> Result<()> {
        #[cfg(not(unix))]
        {
            anyhow::bail!("descriptor-relative recursive removal is unavailable")
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;
            let root_device = self.directory.metadata()?.dev();
            self.remove_contents_on_device(root_device)?;
            self.directory.sync_all()?;
            Ok(())
        }
    }

    /// Bounded variant of recursive removal for an untrusted or authored
    /// generation. The raw namespace budget is shared across every level and
    /// enforced before names are retained.
    pub fn remove_contents_recursive_bounded(
        &self,
        budget: DirectoryTraversalBudget,
    ) -> Result<()> {
        #[cfg(not(unix))]
        {
            let _ = budget;
            anyhow::bail!("descriptor-relative recursive removal is unavailable")
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;
            let root_device = self.directory.metadata()?.dev();
            let mut remaining = budget.max_entries;
            self.remove_contents_on_device_bounded(
                root_device,
                &mut remaining,
                budget.max_depth,
                0,
            )?;
            self.directory.sync_all()?;
            Ok(())
        }
    }

    #[cfg(unix)]
    fn remove_contents_on_device_bounded(
        &self,
        root_device: u64,
        remaining: &mut usize,
        max_depth: usize,
        depth: usize,
    ) -> Result<()> {
        use std::os::unix::ffi::OsStrExt as _;
        use std::os::unix::fs::MetadataExt as _;

        if depth > max_depth {
            anyhow::bail!("recursive removal exceeds its directory depth bound");
        }
        let entries = self.entries_no_follow_bounded(*remaining)?;
        *remaining = remaining
            .checked_sub(entries.len())
            .ok_or_else(|| anyhow::anyhow!("recursive removal entry budget underflow"))?;
        for entry in entries {
            let name_c = std::ffi::CString::new(entry.name.as_bytes())?;
            if entry.entry_type == PinnedEntryType::Directory {
                if depth >= max_depth {
                    anyhow::bail!("recursive removal exceeds its directory depth bound");
                }
                #[cfg(target_os = "linux")]
                {
                    // Removing an empty directory requires access to its
                    // parent, not read access to the child. OverlayFS leaves
                    // mode-000 work directories: pin them without opening
                    // their contents, and let rmdir prove emptiness. Never
                    // chmod an entry or follow an alias to make cleanup pass.
                    let pinned = self
                        .open_mount_entry(&entry.name)?
                        .ok_or_else(|| anyhow::anyhow!("directory disappeared during removal"))?;
                    let child = Self::from_open_directory(self.path.join(&entry.name), pinned)?;
                    if child.directory.metadata()?.dev() != root_device {
                        anyhow::bail!("refusing to cross mounted filesystem during removal");
                    }
                    if self.remove_empty_child_if_same(&entry.name, &child)? {
                        continue;
                    }
                }
                let child = self
                    .open_child_directory(&entry.name)?
                    .ok_or_else(|| anyhow::anyhow!("directory disappeared during removal"))?;
                if child.directory.metadata()?.dev() != root_device {
                    anyhow::bail!(
                        "refusing to cross mounted filesystem while removing {}",
                        child.path.display()
                    );
                }
                child.remove_contents_on_device_bounded(
                    root_device,
                    remaining,
                    max_depth,
                    depth + 1,
                )?;
                if !self.remove_empty_child_if_same(&entry.name, &child)? {
                    anyhow::bail!("directory remained non-empty: {}", child.path.display());
                }
            } else if unsafe { libc::unlinkat(self.directory.as_raw_fd(), name_c.as_ptr(), 0) } != 0
            {
                return Err(std::io::Error::last_os_error()).with_context(|| {
                    format!(
                        "remove pinned entry {}",
                        self.path.join(&entry.name).display()
                    )
                });
            }
        }
        self.directory.sync_all()?;
        Ok(())
    }

    #[cfg(unix)]
    fn remove_contents_on_device(&self, root_device: u64) -> Result<()> {
        use std::os::unix::ffi::OsStrExt as _;
        use std::os::unix::fs::MetadataExt as _;

        for entry in self.entries_no_follow()? {
            let name_c = std::ffi::CString::new(entry.name.as_bytes())?;
            if entry.entry_type == PinnedEntryType::Directory {
                let child = self
                    .open_child_directory(&entry.name)?
                    .ok_or_else(|| anyhow::anyhow!("directory disappeared during removal"))?;
                if child.directory.metadata()?.dev() != root_device {
                    anyhow::bail!(
                        "refusing to cross mounted filesystem while removing {}",
                        child.path.display()
                    );
                }
                child.remove_contents_on_device(root_device)?;
                if !self.remove_empty_child_if_same(&entry.name, &child)? {
                    anyhow::bail!("directory remained non-empty: {}", child.path.display());
                }
            } else if unsafe { libc::unlinkat(self.directory.as_raw_fd(), name_c.as_ptr(), 0) } != 0
            {
                return Err(std::io::Error::last_os_error()).with_context(|| {
                    format!(
                        "remove pinned entry {}",
                        self.path.join(&entry.name).display()
                    )
                });
            }
        }
        self.directory.sync_all()?;
        Ok(())
    }

    pub fn open(path: &Path) -> Result<Option<Self>> {
        #[cfg(not(unix))]
        {
            let _ = path;
            anyhow::bail!("secure directory opening is unavailable on this platform");
        }
        #[cfg(unix)]
        {
            Ok(open_directory_no_follow(path)?.map(|directory| Self {
                path: path.to_path_buf(),
                directory,
            }))
        }
    }

    pub fn open_or_create(path: &Path) -> Result<Self> {
        #[cfg(not(unix))]
        {
            let _ = path;
            anyhow::bail!("secure directory creation is unavailable on this platform");
        }
        #[cfg(unix)]
        {
            Ok(Self {
                path: path.to_path_buf(),
                directory: open_or_create_directory_no_follow(path)?,
            })
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Return the concrete filesystem identity pinned by this descriptor.
    /// Callers use this to compare a durable authority fence with the object
    /// they will actually traverse, without reopening the pathname.
    pub fn device_inode(&self) -> Result<(u64, u64)> {
        #[cfg(not(unix))]
        anyhow::bail!("secure directory identity is unavailable on this platform");
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;
            let metadata = self.directory.metadata()?;
            Ok((metadata.dev(), metadata.ino()))
        }
    }

    /// Capacity observed through this exact filesystem descriptor.
    ///
    /// Storage policy remains caller-owned; this is only the descriptor-safe
    /// primitive needed to enforce a caller's budget without resolving an
    /// ambient pathname after admission.
    pub fn filesystem_capacity(&self) -> Result<FilesystemCapacity> {
        #[cfg(not(unix))]
        anyhow::bail!("descriptor-relative filesystem capacity is unavailable on this platform");
        #[cfg(unix)]
        {
            let mut stats = std::mem::MaybeUninit::<libc::statvfs>::zeroed();
            if unsafe { libc::fstatvfs(self.directory.as_raw_fd(), stats.as_mut_ptr()) } != 0 {
                return Err(std::io::Error::last_os_error())
                    .context("read descriptor-relative filesystem capacity");
            }
            let stats = unsafe { stats.assume_init() };
            let fragment_size = (stats.f_frsize as u64).max(1);
            Ok(FilesystemCapacity {
                total_bytes: (stats.f_blocks as u64).saturating_mul(fragment_size),
                available_bytes: (stats.f_bavail as u64).saturating_mul(fragment_size),
                allocation_unit_bytes: fragment_size,
                available_files: stats.f_favail as u64,
            })
        }
    }

    /// Linux descriptor-rooted child pathname for APIs (notably SQLite) that
    /// cannot accept an already-open directory handle. The child remains bound
    /// to this directory inode even if its ordinary pathname is replaced.
    pub fn descriptor_child_path(&self, name: &OsStr) -> Result<PathBuf> {
        #[cfg(not(target_os = "linux"))]
        {
            let _ = name;
            anyhow::bail!("descriptor-rooted paths are unavailable on this platform");
        }
        #[cfg(target_os = "linux")]
        {
            validate_child_name(name)?;
            Ok(PathBuf::from(format!("/proc/self/fd/{}", self.directory.as_raw_fd())).join(name))
        }
    }

    /// Linux descriptor-rooted pathname for APIs that must walk this exact
    /// already-open directory rather than resolving its ambient path again.
    pub fn descriptor_path(&self) -> Result<PathBuf> {
        #[cfg(not(target_os = "linux"))]
        {
            anyhow::bail!("descriptor-rooted paths are unavailable on this platform");
        }
        #[cfg(target_os = "linux")]
        {
            use std::os::fd::AsRawFd as _;
            Ok(PathBuf::from(format!(
                "/proc/self/fd/{}",
                self.directory.as_raw_fd()
            )))
        }
    }

    /// Duplicate this exact open directory descriptor without resolving its
    /// pathname again.
    pub fn try_clone(&self) -> Result<Self> {
        Ok(Self {
            path: self.path.clone(),
            directory: self.directory.try_clone()?,
        })
    }

    /// Duplicate the open descriptor for APIs that need to bind an operation
    /// to this exact directory inode without resolving its pathname again.
    pub fn try_clone_descriptor(&self) -> Result<File> {
        self.directory
            .try_clone()
            .with_context(|| format!("duplicate pinned directory {}", self.path.display()))
    }

    /// Prove that this pinned inode is still selected by the pathname through
    /// which it was opened. Callers use this immediately before publishing
    /// facts that attribute descriptor-read content to that stable path.
    pub fn ensure_path_binding(&self) -> Result<()> {
        let current = Self::open(&self.path)?.ok_or_else(|| {
            anyhow::anyhow!("pinned directory path disappeared: {}", self.path.display())
        })?;
        if !self.is_same_directory(&current)? {
            anyhow::bail!(
                "pinned directory path was rebound during the operation: {}",
                self.path.display()
            );
        }
        Ok(())
    }

    /// Prove this absolute, no-follow spelling selects this exact directory
    /// and traverses only read-only mounts, including every ancestor. The
    /// caller must already confine subsequent mount changes. Writable views
    /// intentionally cannot be converted to immutable namespace authority.
    pub fn verified_read_only_namespace_path(&self) -> Result<PathBuf> {
        verified_read_only_namespace_path(&self.path, &self.directory)
    }

    /// Prove a stable writable mountpoint selects this exact pinned directory.
    /// Every ancestor must be mounted read-only, and the final directory must
    /// be mounted writable. No component is followed through a symlink.
    ///
    /// The source's diagnostic path may differ from `destination`, or have
    /// been renamed: authority is the exact held directory, not that path.
    /// Only the leaf directory identity is stable; its contents remain mutable.
    /// The caller must already confine future mount changes and separately
    /// admit the destination. This method creates neither mounts nor grants.
    pub fn verified_writable_mount_namespace_path(&self, destination: &Path) -> Result<PathBuf> {
        verified_namespace_path(
            destination,
            &self.directory,
            NamespaceLeafAccess::WritableDirectory,
        )
    }

    /// Walk regular files beneath this exact pinned root. Every child is
    /// opened descriptor-relative without following links; `prune` receives
    /// canonical relative components and may skip a file or whole directory.
    pub fn visit_regular_files<P, V>(&self, mut prune: P, mut visit: V) -> Result<()>
    where
        P: FnMut(&Path, bool) -> Result<bool>,
        V: FnMut(&Path, File) -> Result<()>,
    {
        #[cfg(not(unix))]
        {
            let _ = (&mut prune, &mut visit);
            anyhow::bail!("descriptor-relative traversal is unavailable on this platform")
        }
        #[cfg(unix)]
        visit_from_open_directory(
            &self.path,
            Path::new(""),
            &self.directory,
            None,
            0,
            &mut prune,
            &mut visit,
        )
    }

    /// Bounded form of [`Self::visit_regular_files`]. The traversal budget is
    /// enforced inside Lillux and counts directories, regular files, pruned
    /// entries, and unsupported entries alike.
    pub fn visit_regular_files_bounded<P, V>(
        &self,
        budget: DirectoryTraversalBudget,
        mut prune: P,
        mut visit: V,
    ) -> Result<()>
    where
        P: FnMut(&Path, bool) -> Result<bool>,
        V: FnMut(&Path, File) -> Result<()>,
    {
        #[cfg(not(unix))]
        {
            let _ = (budget, &mut prune, &mut visit);
            anyhow::bail!("descriptor-relative traversal is unavailable on this platform")
        }
        #[cfg(unix)]
        {
            let mut state = DirectoryTraversalState {
                remaining_entries: budget.max_entries,
                max_depth: budget.max_depth,
            };
            visit_from_open_directory(
                &self.path,
                Path::new(""),
                &self.directory,
                Some(&mut state),
                0,
                &mut prune,
                &mut visit,
            )
        }
    }

    /// Copy this exact directory tree into an already-created empty pinned
    /// directory. Every source entry is opened descriptor-relative with
    /// no-follow semantics, every destination entry is created exclusively,
    /// and the source namespace is re-observed before returning. `exclude`
    /// owns product policy only; Lillux owns traversal, identity, metadata,
    /// and publication mechanics.
    pub fn copy_contents_to_filtered<P>(
        &self,
        destination: &PinnedDirectory,
        budget: DirectoryTraversalBudget,
        mut exclude: P,
    ) -> Result<()>
    where
        P: FnMut(&Path) -> Result<bool>,
    {
        #[cfg(not(unix))]
        {
            let _ = (destination, budget, &mut exclude);
            anyhow::bail!("descriptor-relative tree copying is unavailable on this platform")
        }
        #[cfg(unix)]
        {
            let mut state = DirectoryTraversalState {
                remaining_entries: budget.max_entries,
                max_depth: budget.max_depth,
            };
            copy_open_directory_filtered(
                self,
                destination,
                Path::new(""),
                0,
                &mut state,
                &mut exclude,
            )?;
            self.ensure_path_binding()?;
            destination.ensure_path_binding()?;
            Ok(())
        }
    }

    /// Enumerate immediate children from the pinned directory descriptor and
    /// classify each entry without following links.
    #[cfg(unix)]
    pub fn entries_no_follow(&self) -> Result<Vec<PinnedDirectoryEntryMetadata>> {
        self.entries_no_follow_with_limit(None)
    }

    /// Bounded immediate-child enumeration. The limit is enforced while
    /// directory entries are read, before a caller can allocate or filter an
    /// unbounded namespace.
    #[cfg(unix)]
    pub fn entries_no_follow_bounded(
        &self,
        max_entries: usize,
    ) -> Result<Vec<PinnedDirectoryEntryMetadata>> {
        self.entries_no_follow_with_limit(Some(max_entries))
    }

    #[cfg(unix)]
    fn entries_no_follow_with_limit(
        &self,
        max_entries: Option<usize>,
    ) -> Result<Vec<PinnedDirectoryEntryMetadata>> {
        let mut entries = Vec::new();
        let names = match max_entries {
            Some(max_entries) => directory_names_with_limit(&self.directory, max_entries)?,
            None => directory_names(&self.directory)?,
        };
        for name in names {
            let c_name = std::ffi::CString::new(name.as_bytes())?;
            let mut stat: libc::stat = unsafe { std::mem::zeroed() };
            if unsafe {
                libc::fstatat(
                    self.directory.as_raw_fd(),
                    c_name.as_ptr(),
                    &mut stat,
                    libc::AT_SYMLINK_NOFOLLOW,
                )
            } != 0
            {
                return Err(std::io::Error::last_os_error()).with_context(|| {
                    format!(
                        "inspect pinned directory entry {}",
                        self.path.join(&name).display()
                    )
                });
            }
            let entry_type = match stat.st_mode & libc::S_IFMT {
                libc::S_IFDIR => PinnedEntryType::Directory,
                libc::S_IFREG => PinnedEntryType::Regular,
                libc::S_IFLNK => PinnedEntryType::Symlink,
                libc::S_IFCHR => PinnedEntryType::CharacterDevice,
                libc::S_IFBLK => PinnedEntryType::BlockDevice,
                libc::S_IFIFO => PinnedEntryType::Fifo,
                libc::S_IFSOCK => PinnedEntryType::Socket,
                _ => PinnedEntryType::Other,
            };
            entries.push(PinnedDirectoryEntryMetadata {
                name,
                entry_type,
                mode: stat.st_mode,
                device_id: stat.st_rdev,
                containing_device: stat.st_dev,
                inode: stat.st_ino,
            });
        }
        entries.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(entries)
    }

    /// Inspect one immediate child without following it. This is the O(1)
    /// descriptor-relative counterpart to [`Self::entries_no_follow`].
    #[cfg(unix)]
    pub fn entry_no_follow(&self, name: &OsStr) -> Result<Option<PinnedDirectoryEntryMetadata>> {
        validate_child_name(name)?;
        let name_c = std::ffi::CString::new(name.as_bytes())?;
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe {
            libc::fstatat(
                self.directory.as_raw_fd(),
                name_c.as_ptr(),
                &mut stat,
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } != 0
        {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ENOENT) {
                return Ok(None);
            }
            return Err(error).with_context(|| {
                format!(
                    "inspect pinned directory entry {}",
                    self.path.join(name).display()
                )
            });
        }
        let entry_type = match stat.st_mode & libc::S_IFMT {
            libc::S_IFDIR => PinnedEntryType::Directory,
            libc::S_IFREG => PinnedEntryType::Regular,
            libc::S_IFLNK => PinnedEntryType::Symlink,
            libc::S_IFCHR => PinnedEntryType::CharacterDevice,
            libc::S_IFBLK => PinnedEntryType::BlockDevice,
            libc::S_IFIFO => PinnedEntryType::Fifo,
            libc::S_IFSOCK => PinnedEntryType::Socket,
            _ => PinnedEntryType::Other,
        };
        Ok(Some(PinnedDirectoryEntryMetadata {
            name: name.to_os_string(),
            entry_type,
            mode: stat.st_mode,
            device_id: stat.st_rdev,
            containing_device: stat.st_dev,
            inode: stat.st_ino,
        }))
    }

    #[cfg(unix)]
    pub fn ensure_entry_observation(&self, expected: &PinnedDirectoryEntryMetadata) -> Result<()> {
        let observed = self
            .entry_no_follow(&expected.name)?
            .ok_or_else(|| anyhow::anyhow!("pinned directory entry disappeared"))?;
        if &observed != expected {
            anyhow::bail!("pinned directory entry changed while it was being observed");
        }
        Ok(())
    }

    /// Reassert owner-only access on this exact open directory and prove that
    /// its original path still selects the same inode.
    ///
    /// This is the live-directory counterpart to the bounded tree validators
    /// below. It intentionally does not enumerate children: a process that
    /// owns a mutable state directory may create, replace, or remove entries
    /// concurrently, so such a traversal cannot honestly claim a stable tree
    /// snapshot. The pinned root remains the confidentiality boundary.
    pub fn tighten_owner_private_directory(&self) -> Result<()> {
        #[cfg(not(unix))]
        {
            anyhow::bail!("owner-private directory protection is unavailable on this platform")
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;

            self.set_mode(0o700)?;
            let metadata = self.directory.metadata()?;
            if !metadata.is_dir() || metadata.mode() & 0o7777 != 0o700 {
                anyhow::bail!("pinned directory is not exactly owner-private and accessible");
            }
            self.ensure_path_binding()
        }
    }

    /// Validate one bounded owner-private tree without following links.
    ///
    /// Directory and regular-file permission interpretation, device/inode
    /// identity, and no-follow platform mechanics remain inside Lillux.
    /// Symlinks consume the namespace budget and retain exact no-follow inode
    /// identity, but their target and conventional mode bits are never read.
    /// Special entries and mounted-filesystem crossings fail closed. The
    /// returned count includes regular-file bytes only.
    pub fn require_owner_private_tree_bounded(
        &self,
        budget: DirectoryTraversalBudget,
        maximum_regular_bytes: u64,
    ) -> Result<u64> {
        self.owner_private_tree_bounded(
            budget,
            maximum_regular_bytes,
            OwnerPrivateTreeAction::Require,
        )
    }

    /// Validate one bounded opaque tree enclosed by an exact owner-private root.
    ///
    /// The opened root is the confidentiality boundary and must remain exactly
    /// mode 0700. Descendant directories and regular files must have the same
    /// owner as that root, but their group/other mode bits are workload state:
    /// they grant no access through an untraversable root and are not rewritten
    /// or treated as RyeOS credential metadata. Links remain opaque and are
    /// never followed. Device, inode, namespace, hard-link, special-entry,
    /// depth, entry-count, and byte limits retain the strict tree walk above.
    pub fn require_owner_enclosed_tree_bounded(
        &self,
        budget: DirectoryTraversalBudget,
        maximum_regular_bytes: u64,
    ) -> Result<u64> {
        self.owner_private_tree_bounded(
            budget,
            maximum_regular_bytes,
            OwnerPrivateTreeAction::RequireEnclosed,
        )
    }

    /// Tighten one bounded tree to owner-private permissions without following
    /// links, returning its regular-file byte count.
    ///
    /// Directories become owner-only and owner-accessible. Regular files keep
    /// their owner permission class while group/other and special permission
    /// bits are removed. Symlinks remain opaque workload state. Identity,
    /// namespace, device, depth, entry-count, and byte limits are checked with
    /// the same guarantees as [`Self::require_owner_private_tree_bounded`].
    pub fn tighten_owner_private_tree_bounded(
        &self,
        budget: DirectoryTraversalBudget,
        maximum_regular_bytes: u64,
    ) -> Result<u64> {
        self.owner_private_tree_bounded(
            budget,
            maximum_regular_bytes,
            OwnerPrivateTreeAction::Tighten,
        )
    }

    fn owner_private_tree_bounded(
        &self,
        budget: DirectoryTraversalBudget,
        maximum_regular_bytes: u64,
        action: OwnerPrivateTreeAction,
    ) -> Result<u64> {
        #[cfg(not(unix))]
        {
            let _ = (budget, maximum_regular_bytes, action);
            anyhow::bail!("owner-private tree traversal is unavailable on this platform")
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;

            fn same_entry_identity(
                left: &PinnedDirectoryEntryMetadata,
                right: &PinnedDirectoryEntryMetadata,
            ) -> bool {
                left.name == right.name
                    && left.entry_type == right.entry_type
                    && left.containing_device == right.containing_device
                    && left.inode == right.inode
            }

            fn require_owner_accessible_directory(metadata: &std::fs::Metadata) -> Result<()> {
                if !metadata.is_dir() || metadata.mode() & 0o700 != 0o700 {
                    anyhow::bail!("owner-private tree directory is not owner-accessible");
                }
                Ok(())
            }

            fn require_owner_private_permissions(metadata: &std::fs::Metadata) -> Result<()> {
                if metadata.mode() & 0o077 != 0 {
                    anyhow::bail!("owner-private tree entry grants group or other permissions");
                }
                Ok(())
            }

            fn require_exact_owner_private_root(metadata: &std::fs::Metadata) -> Result<()> {
                if !metadata.is_dir() || metadata.mode() & 0o7777 != 0o700 {
                    anyhow::bail!("owner-enclosed tree root is not exactly mode 0700");
                }
                Ok(())
            }

            fn require_same_owner(metadata: &std::fs::Metadata, expected_owner: u32) -> Result<()> {
                if metadata.uid() != expected_owner {
                    anyhow::bail!("owner-enclosed tree entry has a different owner");
                }
                Ok(())
            }

            fn require_private_regular_identity(metadata: &std::fs::Metadata) -> Result<()> {
                if !metadata.is_file() || metadata.nlink() != 1 {
                    anyhow::bail!(
                        "owner-private tree regular file is not confined to one namespace link"
                    );
                }
                Ok(())
            }

            fn require_current_entry(
                parent: &PinnedDirectory,
                expected: &PinnedDirectoryEntryMetadata,
            ) -> Result<PinnedDirectoryEntryMetadata> {
                let current = parent
                    .entry_no_follow(&expected.name)?
                    .ok_or_else(|| anyhow::anyhow!("owner-private tree entry disappeared"))?;
                if !same_entry_identity(expected, &current) {
                    anyhow::bail!("owner-private tree entry changed identity");
                }
                Ok(current)
            }

            #[allow(clippy::too_many_arguments)]
            fn visit(
                directory: &PinnedDirectory,
                root_device: u64,
                remaining_entries: &mut usize,
                max_depth: usize,
                depth: usize,
                maximum_regular_bytes: u64,
                regular_bytes: &mut u64,
                action: OwnerPrivateTreeAction,
                root_owner: u32,
            ) -> Result<()> {
                if depth > max_depth {
                    anyhow::bail!("owner-private tree reached its directory-depth ceiling");
                }
                let initial = directory.entries_no_follow_bounded(*remaining_entries)?;
                *remaining_entries = remaining_entries
                    .checked_sub(initial.len())
                    .ok_or_else(|| anyhow::anyhow!("owner-private tree entry budget underflow"))?;

                for entry in &initial {
                    if entry.containing_device != root_device {
                        anyhow::bail!("owner-private tree crosses a mounted filesystem");
                    }
                    match entry.entry_type {
                        PinnedEntryType::Symlink => {
                            directory.ensure_entry_observation(entry)?;
                        }
                        PinnedEntryType::Directory => {
                            let child =
                                directory
                                    .open_child_directory(&entry.name)?
                                    .ok_or_else(|| {
                                        anyhow::anyhow!("owner-private tree directory disappeared")
                                    })?;
                            let identity = child.identity()?;
                            if identity.containing_device != entry.containing_device
                                || identity.inode != entry.inode
                            {
                                anyhow::bail!("owner-private tree directory changed identity");
                            }
                            let before = child.directory.metadata()?;
                            require_owner_accessible_directory(&before)?;
                            match action {
                                OwnerPrivateTreeAction::Require => {
                                    require_owner_private_permissions(&before)?;
                                }
                                OwnerPrivateTreeAction::RequireEnclosed => {
                                    require_same_owner(&before, root_owner)?;
                                }
                                OwnerPrivateTreeAction::Tighten => child.set_mode(0o700)?,
                            }
                            let current = require_current_entry(directory, entry)?;
                            let current_metadata = child.directory.metadata()?;
                            match action {
                                OwnerPrivateTreeAction::RequireEnclosed => {
                                    require_same_owner(&current_metadata, root_owner)?;
                                }
                                OwnerPrivateTreeAction::Require
                                | OwnerPrivateTreeAction::Tighten => {
                                    require_owner_private_permissions(&current_metadata)?;
                                    if current.mode & 0o077 != 0 {
                                        anyhow::bail!(
                                            "owner-private tree directory permissions changed in namespace"
                                        );
                                    }
                                }
                            }
                            visit(
                                &child,
                                root_device,
                                remaining_entries,
                                max_depth,
                                depth + 1,
                                maximum_regular_bytes,
                                regular_bytes,
                                action,
                                root_owner,
                            )?;
                            let current = require_current_entry(directory, entry)?;
                            let after = child.directory.metadata()?;
                            require_owner_accessible_directory(&after)?;
                            match action {
                                OwnerPrivateTreeAction::RequireEnclosed => {
                                    require_same_owner(&after, root_owner)?;
                                }
                                OwnerPrivateTreeAction::Require
                                | OwnerPrivateTreeAction::Tighten => {
                                    require_owner_private_permissions(&after)?;
                                    if current.mode & 0o077 != 0 {
                                        anyhow::bail!(
                                            "owner-private tree directory permissions changed in namespace"
                                        );
                                    }
                                }
                            }
                        }
                        PinnedEntryType::Regular => {
                            let file =
                                directory.open_regular(&entry.name, false)?.ok_or_else(|| {
                                    anyhow::anyhow!("owner-private tree file disappeared")
                                })?;
                            let before = observe_open_regular_file(&file)?;
                            if !before.matches_directory_entry(entry) {
                                anyhow::bail!("owner-private tree file changed identity");
                            }
                            let before_metadata = file.metadata()?;
                            require_private_regular_identity(&before_metadata)?;
                            match action {
                                OwnerPrivateTreeAction::Require => {
                                    require_owner_private_permissions(&before_metadata)?;
                                }
                                OwnerPrivateTreeAction::RequireEnclosed => {
                                    require_same_owner(&before_metadata, root_owner)?;
                                }
                                OwnerPrivateTreeAction::Tighten => {
                                    set_open_regular_file_mode(
                                        &file,
                                        before_metadata.mode() & 0o700,
                                    )?;
                                }
                            }
                            let after_metadata = file.metadata()?;
                            require_private_regular_identity(&after_metadata)?;
                            let current = require_current_entry(directory, entry)?;
                            match action {
                                OwnerPrivateTreeAction::RequireEnclosed => {
                                    require_same_owner(&after_metadata, root_owner)?;
                                }
                                OwnerPrivateTreeAction::Require
                                | OwnerPrivateTreeAction::Tighten => {
                                    require_owner_private_permissions(&after_metadata)?;
                                    if current.mode & 0o077 != 0 {
                                        anyhow::bail!(
                                            "owner-private tree file permissions changed in namespace"
                                        );
                                    }
                                }
                            }
                            *regular_bytes =
                                regular_bytes.checked_add(after_metadata.len()).ok_or_else(
                                    || anyhow::anyhow!("owner-private tree byte count overflow"),
                                )?;
                            if *regular_bytes > maximum_regular_bytes {
                                anyhow::bail!("owner-private tree reached its byte ceiling");
                            }
                        }
                        _ => anyhow::bail!("owner-private tree contains a special entry"),
                    }
                }

                let final_entries = directory.entries_no_follow_bounded(initial.len())?;
                if final_entries.len() != initial.len()
                    || initial
                        .iter()
                        .zip(&final_entries)
                        .any(|(before, after)| !same_entry_identity(before, after))
                {
                    anyhow::bail!("owner-private tree namespace changed during traversal");
                }
                Ok(())
            }

            let metadata = self.directory.metadata()?;
            require_owner_accessible_directory(&metadata)?;
            match action {
                OwnerPrivateTreeAction::Require => {
                    require_owner_private_permissions(&metadata)?;
                }
                OwnerPrivateTreeAction::RequireEnclosed => {
                    require_exact_owner_private_root(&metadata)?;
                }
                OwnerPrivateTreeAction::Tighten => self.set_mode(0o700)?,
            }
            let root_device = self.directory.metadata()?.dev();
            let root_owner = self.directory.metadata()?.uid();
            let mut remaining_entries = budget.max_entries;
            let mut regular_bytes = 0;
            visit(
                self,
                root_device,
                &mut remaining_entries,
                budget.max_depth,
                0,
                maximum_regular_bytes,
                &mut regular_bytes,
                action,
                root_owner,
            )?;
            let final_root = self.directory.metadata()?;
            require_owner_accessible_directory(&final_root)?;
            match action {
                OwnerPrivateTreeAction::RequireEnclosed => {
                    require_exact_owner_private_root(&final_root)?;
                    require_same_owner(&final_root, root_owner)?;
                }
                OwnerPrivateTreeAction::Require | OwnerPrivateTreeAction::Tighten => {
                    require_owner_private_permissions(&final_root)?;
                }
            }
            self.ensure_path_binding()?;
            Ok(regular_bytes)
        }
    }

    /// Read a bounded extended-attribute value from the pinned directory.
    #[cfg(target_os = "linux")]
    pub fn xattr(&self, name: &std::ffi::CStr, max_bytes: usize) -> Result<Option<Vec<u8>>> {
        let size = unsafe {
            libc::fgetxattr(
                self.directory.as_raw_fd(),
                name.as_ptr(),
                std::ptr::null_mut(),
                0,
            )
        };
        if size < 0 {
            let error = std::io::Error::last_os_error();
            if error
                .raw_os_error()
                .is_some_and(|code| code == libc::ENODATA || code == libc::ENOTSUP)
            {
                return Ok(None);
            }
            return Err(error).context("read pinned directory extended attribute size");
        }
        let size = usize::try_from(size).context("extended attribute size overflow")?;
        if size > max_bytes {
            anyhow::bail!("extended attribute exceeds {max_bytes} bytes");
        }
        let mut value = vec![0_u8; size];
        let read = unsafe {
            libc::fgetxattr(
                self.directory.as_raw_fd(),
                name.as_ptr(),
                value.as_mut_ptr().cast(),
                value.len(),
            )
        };
        if read < 0 {
            return Err(std::io::Error::last_os_error())
                .context("read pinned directory extended attribute");
        }
        value.truncate(usize::try_from(read).context("extended attribute length overflow")?);
        Ok(Some(value))
    }

    /// Set permissions on this exact open directory inode.
    pub fn set_mode(&self, mode: u32) -> Result<()> {
        #[cfg(not(unix))]
        {
            let _ = mode;
            anyhow::bail!("pinned directory permissions are unavailable on this platform");
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            self.directory
                .set_permissions(std::fs::Permissions::from_mode(mode))
                .with_context(|| format!("protect pinned directory {}", self.path.display()))
        }
    }

    #[cfg(unix)]
    fn independent_lock_descriptor(&self) -> Result<File> {
        // dup/try_clone shares an open-file-description, so two acquisitions
        // on the same PinnedDirectory would both succeed and either unlock
        // would release the other's gate. Open "." relative to the retained
        // descriptor: a new lock description for the exact same inode, never
        // an ambient reopen of self.path(). Cloning an acquired guard remains
        // intentionally shared through its existing Arc.
        open_child_directory(&self.directory, c".", &self.path)?
            .context("pinned directory disappeared while opening its lock description")
    }

    /// Serialize cooperating mutations of this exact directory namespace.
    pub fn lock_exclusive(&self) -> Result<PinnedDirectoryLock> {
        #[cfg(not(unix))]
        anyhow::bail!("pinned directory locking is unavailable on this platform");
        #[cfg(unix)]
        {
            let file = self.independent_lock_descriptor()?;
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
                return Err(std::io::Error::last_os_error())
                    .with_context(|| format!("lock pinned directory {}", self.path.display()));
            }
            Ok(PinnedDirectoryLock {
                inner: Arc::new(PinnedDirectoryLockInner { file }),
            })
        }
    }

    /// Attempt to serialize cooperating mutations of this exact directory
    /// namespace. `Ok(None)` is exclusively the ordinary contention result;
    /// callers do not need to interpret a native lock error or descriptor.
    pub fn try_lock_exclusive(&self) -> Result<Option<PinnedDirectoryLock>> {
        #[cfg(not(unix))]
        anyhow::bail!("pinned directory locking is unavailable on this platform");
        #[cfg(unix)]
        {
            let file = self.independent_lock_descriptor()?;
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                return Ok(Some(PinnedDirectoryLock {
                    inner: Arc::new(PinnedDirectoryLockInner { file }),
                }));
            }
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::WouldBlock
                || error.raw_os_error() == Some(libc::EWOULDBLOCK)
            {
                Ok(None)
            } else {
                Err(error).with_context(|| format!("lock pinned directory {}", self.path.display()))
            }
        }
    }

    /// Serialize cooperating mutations of this exact directory namespace,
    /// failing after a bounded monotonic wait. The lock is held on the pinned
    /// directory inode itself and creates no namespace entry.
    pub fn lock_exclusive_with_timeout(
        &self,
        timeout: crate::time::Duration,
    ) -> Result<PinnedDirectoryLock> {
        #[cfg(not(unix))]
        {
            let _ = timeout;
            anyhow::bail!("pinned directory locking is unavailable on this platform");
        }
        #[cfg(unix)]
        {
            let file = self.independent_lock_descriptor()?;
            let started = crate::time::MonotonicTimer::start();
            loop {
                if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                    return Ok(PinnedDirectoryLock {
                        inner: Arc::new(PinnedDirectoryLockInner { file }),
                    });
                }
                let error = std::io::Error::last_os_error();
                if error.kind() != std::io::ErrorKind::WouldBlock {
                    return Err(error)
                        .with_context(|| format!("lock pinned directory {}", self.path.display()));
                }
                let elapsed = started.elapsed();
                if elapsed >= timeout {
                    let holder = crate::locks::linux_flock_holder_pid(&file)
                        .map(|pid| pid.to_string())
                        .unwrap_or_else(|| "unknown".to_string());
                    anyhow::bail!(
                        "timed out after {:.1}s waiting to lock pinned directory {} (holder pid: {holder})",
                        timeout.as_secs_f64(),
                        self.path.display()
                    );
                }
                std::thread::sleep(
                    crate::time::Duration::from_millis(50).min(timeout.saturating_sub(elapsed)),
                );
            }
        }
    }

    /// Compare the concrete directory inodes behind two independently pinned
    /// paths. This lets lock holders prove that the namespace used for a later
    /// mutation is still the directory in which the held lock was acquired.
    pub fn is_same_directory(&self, other: &Self) -> Result<bool> {
        #[cfg(not(unix))]
        {
            let _ = other;
            anyhow::bail!("secure directory identity is unavailable on this platform");
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let left = self.directory.metadata()?;
            let right = other.directory.metadata()?;
            Ok(left.dev() == right.dev() && left.ino() == right.ino())
        }
    }

    /// Open or create one child directory relative to this exact parent inode.
    pub fn open_or_create_child(&self, name: &OsStr, mode: u32) -> Result<Self> {
        #[cfg(not(unix))]
        {
            let _ = (name, mode);
            anyhow::bail!("secure child-directory creation is unavailable on this platform");
        }
        #[cfg(unix)]
        {
            validate_child_name(name)?;
            let name_c = std::ffi::CString::new(name.as_bytes())?;
            let path = self.path.join(name);
            if let Some(directory) = open_child_directory(&self.directory, &name_c, &path)? {
                return Ok(Self { path, directory });
            }
            if unsafe { libc::mkdirat(self.directory.as_raw_fd(), name_c.as_ptr(), mode) } != 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() != std::io::ErrorKind::AlreadyExists {
                    return Err(error).with_context(|| {
                        format!("create secure child directory {}", path.display())
                    });
                }
            }
            self.directory.sync_all()?;
            let directory =
                open_child_directory(&self.directory, &name_c, &path)?.ok_or_else(|| {
                    anyhow::anyhow!("secure child directory disappeared: {}", path.display())
                })?;
            Ok(Self { path, directory })
        }
    }

    /// Create one new child directory relative to this exact parent inode.
    pub fn create_child(&self, name: &OsStr, mode: u32) -> Result<Self> {
        #[cfg(not(unix))]
        {
            let _ = (name, mode);
            anyhow::bail!("secure child-directory creation is unavailable on this platform");
        }
        #[cfg(unix)]
        {
            validate_child_name(name)?;
            let name_c = std::ffi::CString::new(name.as_bytes())?;
            let path = self.path.join(name);
            if unsafe { libc::mkdirat(self.directory.as_raw_fd(), name_c.as_ptr(), mode) } != 0 {
                return Err(std::io::Error::last_os_error())
                    .with_context(|| format!("create secure child directory {}", path.display()));
            }
            self.directory.sync_all()?;
            let directory =
                open_child_directory(&self.directory, &name_c, &path)?.ok_or_else(|| {
                    anyhow::anyhow!("secure child directory disappeared: {}", path.display())
                })?;
            Ok(Self { path, directory })
        }
    }

    /// Create one collision-resistant private staging child. Process identity,
    /// entropy, retry, and exact descriptor reopening remain Lillux mechanics;
    /// the caller supplies only a diagnostic namespace prefix and mode.
    pub fn create_unique_child(&self, prefix: &str, mode: u32) -> Result<(OsString, Self)> {
        const MAX_ATTEMPTS: usize = 1_024;
        if prefix.is_empty()
            || prefix.len() > 160
            || prefix.bytes().any(|byte| matches!(byte, 0 | b'/'))
        {
            anyhow::bail!("unique child prefix is not a bounded direct-child name");
        }
        for _ in 0..MAX_ATTEMPTS {
            let name = OsString::from(format!(
                "{prefix}.{}.{}",
                std::process::id(),
                rand::random::<u64>()
            ));
            match self.create_child(&name, mode) {
                Ok(child) => return Ok((name, child)),
                Err(error)
                    if error
                        .downcast_ref::<std::io::Error>()
                        .is_some_and(|error| error.kind() == std::io::ErrorKind::AlreadyExists) => {
                }
                Err(error) => return Err(error),
            }
        }
        anyhow::bail!("could not reserve a unique private staging child")
    }

    /// Atomically exchange two directory children only while both names still
    /// bind the exact pinned directory identities supplied by the caller.
    /// The post-exchange check proves the visible names reversed exactly; a
    /// durability error is reported as committed rather than inviting retry.
    pub fn exchange_child_directories_if_same(
        &self,
        left_name: &OsStr,
        left: &PinnedDirectory,
        right_name: &OsStr,
        right: &PinnedDirectory,
    ) -> crate::atomic_fs::AtomicMutationResult<()> {
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (left_name, left, right_name, right);
            return Err(crate::atomic_fs::AtomicMutationError::before(
                anyhow::anyhow!("conditional directory exchange requires Linux renameat2"),
            ));
        }
        #[cfg(target_os = "linux")]
        {
            use std::os::fd::AsRawFd as _;
            use std::os::unix::ffi::OsStrExt as _;

            validate_child_name(left_name)
                .map_err(crate::atomic_fs::AtomicMutationError::before)?;
            validate_child_name(right_name)
                .map_err(crate::atomic_fs::AtomicMutationError::before)?;
            let left_c = std::ffi::CString::new(left_name.as_bytes())
                .map_err(crate::atomic_fs::AtomicMutationError::before)?;
            let right_c = std::ffi::CString::new(right_name.as_bytes())
                .map_err(crate::atomic_fs::AtomicMutationError::before)?;
            ensure_directory_child_identity(
                &self.directory,
                &left_c,
                left.identity()
                    .map_err(crate::atomic_fs::AtomicMutationError::before)?,
                &self.path.join(left_name),
            )
            .map_err(crate::atomic_fs::AtomicMutationError::before)?;
            ensure_directory_child_identity(
                &self.directory,
                &right_c,
                right
                    .identity()
                    .map_err(crate::atomic_fs::AtomicMutationError::before)?,
                &self.path.join(right_name),
            )
            .map_err(crate::atomic_fs::AtomicMutationError::before)?;
            if unsafe {
                libc::renameat2(
                    self.directory.as_raw_fd(),
                    left_c.as_ptr(),
                    self.directory.as_raw_fd(),
                    right_c.as_ptr(),
                    libc::RENAME_EXCHANGE,
                )
            } != 0
            {
                return Err(crate::atomic_fs::AtomicMutationError::before(
                    std::io::Error::last_os_error(),
                ));
            }
            if let Err(error) = ensure_directory_child_identity(
                &self.directory,
                &left_c,
                right
                    .identity()
                    .map_err(crate::atomic_fs::AtomicMutationError::namespace_changed)?,
                &self.path.join(left_name),
            )
            .and_then(|_| {
                ensure_directory_child_identity(
                    &self.directory,
                    &right_c,
                    left.identity()?,
                    &self.path.join(right_name),
                )
            }) {
                return Err(crate::atomic_fs::AtomicMutationError::namespace_changed(
                    error.context(
                        "conditional directory exchange committed an unexpected namespace",
                    ),
                ));
            }
            self.directory
                .sync_all()
                .map_err(crate::atomic_fs::AtomicMutationError::durability)
        }
    }

    /// Move one exact observed child into another pinned directory without
    /// replacing a destination entry. `Ok(false)` means the destination name
    /// was already occupied and no namespace mutation occurred.
    pub fn move_child_if_same_noreplace_to(
        &self,
        entry: &PinnedDirectoryEntryMetadata,
        destination: &PinnedDirectory,
    ) -> crate::atomic_fs::AtomicMutationResult<bool> {
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (entry, destination);
            return Err(crate::atomic_fs::AtomicMutationError::before(
                anyhow::anyhow!("conditional child move requires Linux renameat2"),
            ));
        }
        #[cfg(target_os = "linux")]
        {
            use std::os::fd::AsRawFd as _;
            use std::os::unix::ffi::OsStrExt as _;

            self.ensure_entry_observation(entry)
                .map_err(crate::atomic_fs::AtomicMutationError::before)?;
            if destination
                .entry_no_follow(&entry.name)
                .map_err(crate::atomic_fs::AtomicMutationError::before)?
                .is_some()
            {
                return Ok(false);
            }
            let name = std::ffi::CString::new(entry.name.as_bytes())
                .map_err(crate::atomic_fs::AtomicMutationError::before)?;
            if unsafe {
                libc::renameat2(
                    self.directory.as_raw_fd(),
                    name.as_ptr(),
                    destination.directory.as_raw_fd(),
                    name.as_ptr(),
                    libc::RENAME_NOREPLACE,
                )
            } != 0
            {
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() == Some(libc::EEXIST)
                    && destination
                        .entry_no_follow(&entry.name)
                        .map_err(crate::atomic_fs::AtomicMutationError::before)?
                        .is_some()
                {
                    return Ok(false);
                }
                return Err(crate::atomic_fs::AtomicMutationError::before(error));
            }
            destination
                .ensure_entry_observation(entry)
                .map_err(|error| {
                    crate::atomic_fs::AtomicMutationError::namespace_changed(
                        error.context("conditional child move published an unexpected identity"),
                    )
                })?;
            self.directory
                .sync_all()
                .and_then(|_| destination.directory.sync_all())
                .map_err(crate::atomic_fs::AtomicMutationError::durability)?;
            Ok(true)
        }
    }

    /// Deterministically enumerate names relative to this exact directory
    /// inode. Callers must subsequently open each name through this handle;
    /// names alone are never authority for a pathname-based operation.
    pub fn entry_names(&self) -> Result<Vec<OsString>> {
        #[cfg(not(target_os = "linux"))]
        anyhow::bail!("pinned directory enumeration is unavailable on this platform");
        #[cfg(target_os = "linux")]
        directory_names_bounded(&self.directory, None)
    }

    /// Enumerate at most `max_entries` names from this exact directory inode.
    /// The returned subset is sorted, but it is intentionally not a global
    /// lexical prefix: callers use this for bounded, repeatable housekeeping
    /// where deleting observed entries allows later passes to make progress.
    pub fn entry_names_bounded(&self, max_entries: usize) -> Result<Vec<OsString>> {
        #[cfg(not(target_os = "linux"))]
        {
            let _ = max_entries;
            anyhow::bail!("pinned directory enumeration is unavailable on this platform");
        }
        #[cfg(target_os = "linux")]
        directory_names_bounded(&self.directory, Some(max_entries))
    }

    /// Open one existing child directory relative to this pinned directory.
    /// No component is followed through a symlink.
    /// Read one child symlink's target without following it.
    ///
    /// Descriptor-relative `readlinkat`, so the lookup cannot be redirected by
    /// swapping a path component after classification. Callers that record a
    /// symlink as content need its target bytes: a digest of the target proves
    /// what was observed but cannot reconstruct the link.
    pub fn read_symlink_target(&self, name: &OsStr, max_bytes: usize) -> Result<Option<Vec<u8>>> {
        #[cfg(not(unix))]
        {
            let _ = (name, max_bytes);
            anyhow::bail!("descriptor-relative symlink reading is unavailable on this platform");
        }
        #[cfg(unix)]
        {
            validate_child_name(name)?;
            let name_c = std::ffi::CString::new(name.as_bytes())?;
            // One extra byte distinguishes "exactly at the bound" from
            // "truncated", so an oversized target fails rather than being
            // silently shortened into a different link.
            let mut buffer = vec![0u8; max_bytes.saturating_add(1)];
            let written = unsafe {
                libc::readlinkat(
                    self.directory.as_raw_fd(),
                    name_c.as_ptr(),
                    buffer.as_mut_ptr() as *mut libc::c_char,
                    buffer.len(),
                )
            };
            if written < 0 {
                let error = std::io::Error::last_os_error();
                return match error.raw_os_error() {
                    Some(libc::ENOENT) => Ok(None),
                    Some(libc::EINVAL) => Ok(None),
                    _ => Err(error).with_context(|| {
                        format!("read symlink target {}", self.path.join(name).display())
                    }),
                };
            }
            let written = written as usize;
            if written > max_bytes {
                anyhow::bail!(
                    "symlink target at {} exceeds {max_bytes} bytes",
                    self.path.join(name).display()
                );
            }
            buffer.truncate(written);
            Ok(Some(buffer))
        }
    }

    /// Create one symlink relative to this exact directory without replacing
    /// an existing entry. The target is opaque bytes: it is never resolved or
    /// followed while the realization is assembled.
    pub fn create_symlink(&self, name: &OsStr, target: &[u8]) -> Result<()> {
        #[cfg(not(unix))]
        {
            let _ = (name, target);
            anyhow::bail!("descriptor-relative symlink creation is unavailable on this platform");
        }
        #[cfg(unix)]
        {
            validate_child_name(name)?;
            if target.is_empty() {
                anyhow::bail!("symlink target must not be empty");
            }
            let name_c = std::ffi::CString::new(name.as_bytes())?;
            let target_c = std::ffi::CString::new(target)?;
            if unsafe {
                libc::symlinkat(
                    target_c.as_ptr(),
                    self.directory.as_raw_fd(),
                    name_c.as_ptr(),
                )
            } != 0
            {
                return Err(std::io::Error::last_os_error()).with_context(|| {
                    format!("create secure symlink {}", self.path.join(name).display())
                });
            }
            self.directory.sync_all()?;
            Ok(())
        }
    }

    pub fn open_child_directory(&self, name: &OsStr) -> Result<Option<Self>> {
        #[cfg(not(unix))]
        {
            let _ = name;
            anyhow::bail!("secure child-directory opening is unavailable on this platform");
        }
        #[cfg(unix)]
        {
            validate_child_name(name)?;
            let name_c = std::ffi::CString::new(name.as_bytes())?;
            let path = self.path.join(name);
            if let Some(directory) = open_child_directory(&self.directory, &name_c, &path)? {
                return Ok(Some(Self { path, directory }));
            }
            if open_regular_at(&self.directory, &name_c, &path)?.is_some() {
                anyhow::bail!(
                    "secure child expected to be a directory but is a regular file: {}",
                    path.display()
                );
            }
            Ok(None)
        }
    }

    /// Open one direct child as either a pinned directory or regular file.
    /// Missing entries return `None`; links and special files fail closed.
    pub fn open_entry(&self, name: &OsStr, writable: bool) -> Result<Option<PinnedDirectoryEntry>> {
        #[cfg(not(unix))]
        {
            let _ = (name, writable);
            anyhow::bail!("secure mixed-entry opening is unavailable on this platform");
        }
        #[cfg(unix)]
        {
            validate_child_name(name)?;
            let name_c = std::ffi::CString::new(name.as_bytes())?;
            let path = self.path.join(name);
            if let Some(directory) = open_child_directory(&self.directory, &name_c, &path)? {
                return Ok(Some(PinnedDirectoryEntry::Directory(Self {
                    path,
                    directory,
                })));
            }
            open_regular_at_flags(
                &self.directory,
                &name_c,
                &path,
                if writable {
                    libc::O_RDWR
                } else {
                    libc::O_RDONLY
                },
                0,
                0,
            )
            .map(|file| file.map(PinnedDirectoryEntry::Regular))
        }
    }

    /// Open one existing regular child relative to this pinned directory.
    pub fn open_regular(&self, name: &OsStr, writable: bool) -> Result<Option<File>> {
        #[cfg(not(unix))]
        {
            let _ = (name, writable);
            anyhow::bail!("secure regular-file opening is unavailable on this platform");
        }
        #[cfg(unix)]
        {
            validate_child_name(name)?;
            let name_c = std::ffi::CString::new(name.as_bytes())?;
            open_regular_at_flags(
                &self.directory,
                &name_c,
                &self.path.join(name),
                if writable {
                    libc::O_RDWR
                } else {
                    libc::O_RDONLY
                },
                0,
                0,
            )
        }
    }

    /// Open one existing regular child and retain both its descriptor and its
    /// descriptor-relative diagnostic identity.
    pub fn open_pinned_regular(
        &self,
        name: &OsStr,
        writable: bool,
    ) -> Result<Option<PinnedRegularFile>> {
        let file = self.open_regular(name, writable)?;
        Ok(file.map(|file| PinnedRegularFile {
            path: self.path.join(name),
            name: name.to_os_string(),
            file,
        }))
    }

    /// Open one normalized regular-file descendant without following any
    /// component through a symlink. The returned authority retains the exact
    /// file descriptor and its descriptor-relative coordinate; callers do not
    /// need to reconstruct traversal from raw `openat` operations.
    pub fn open_pinned_regular_descendant(
        &self,
        relative: &Path,
        writable: bool,
    ) -> Result<Option<PinnedRegularFile>> {
        #[cfg(not(unix))]
        {
            let _ = (relative, writable);
            anyhow::bail!("descriptor-relative descendant opening is unavailable")
        }
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt as _;

            let mut components = relative.components().peekable();
            let mut directory = self
                .directory
                .try_clone()
                .with_context(|| format!("clone pinned directory {}", self.path.display()))?;
            let mut display = self.path.clone();
            while let Some(component) = components.next() {
                let std::path::Component::Normal(name) = component else {
                    anyhow::bail!(
                        "pinned regular descendant path is not normalized: {}",
                        relative.display()
                    );
                };
                if components.peek().is_none() {
                    let name_c = std::ffi::CString::new(name.as_bytes())?;
                    let file = open_regular_at_flags(
                        &directory,
                        &name_c,
                        &display.join(name),
                        if writable {
                            libc::O_RDWR
                        } else {
                            libc::O_RDONLY
                        },
                        0,
                        0,
                    )?;
                    return Ok(file.map(|file| PinnedRegularFile {
                        path: display.join(name),
                        name: name.to_os_string(),
                        file,
                    }));
                }
                let name_c = std::ffi::CString::new(name.as_bytes())?;
                let child_path = display.join(name);
                let Some(child) = open_child_directory(&directory, &name_c, &child_path)? else {
                    return Ok(None);
                };
                directory = child;
                display = child_path;
            }
            anyhow::bail!("pinned regular descendant path is empty")
        }
    }

    /// Consume this exact directory authority into a descriptor-rooted child
    /// path without reopening its ambient namespace name.
    pub fn into_inherited_descriptor_path(
        self,
    ) -> Result<crate::exec::InheritedDescriptorAuthority> {
        crate::exec::inherited_descriptor_path(self.directory).map_err(anyhow::Error::msg)
    }

    pub fn inherited_descriptor_authority(
        &self,
    ) -> Result<crate::exec::InheritedDescriptorAuthority> {
        #[cfg(not(unix))]
        anyhow::bail!("inherited descriptor authority is unavailable on this platform");
        #[cfg(unix)]
        {
            let lease = crate::exec::retain_fork_sensitive_descriptors();
            let file = self.directory.try_clone()?;
            crate::exec::InheritedDescriptorAuthority::from_owned_file(file, &lease)
                .map_err(anyhow::Error::msg)
        }
    }

    /// Open and register an exact child mount descriptor before another
    /// attachment may fork. Ordinary byte-stream callers keep open_regular.
    pub fn open_inherited_mount_entry(
        &self,
        name: &OsStr,
    ) -> Result<Option<crate::exec::InheritedDescriptorAuthority>> {
        #[cfg(not(unix))]
        {
            let _ = name;
            anyhow::bail!("inherited mount authority is unavailable on this platform");
        }
        #[cfg(unix)]
        {
            let lease = crate::exec::retain_fork_sensitive_descriptors();
            self.open_mount_entry(name)?
                .map(|file| {
                    crate::exec::InheritedDescriptorAuthority::from_owned_file(file, &lease)
                        .map_err(anyhow::Error::msg)
                })
                .transpose()
        }
    }

    pub fn open_inherited_regular(
        &self,
        name: &OsStr,
        writable: bool,
    ) -> Result<Option<crate::exec::InheritedDescriptorAuthority>> {
        #[cfg(not(unix))]
        {
            let _ = (name, writable);
            anyhow::bail!("inherited descriptor authority is unavailable on this platform");
        }
        #[cfg(unix)]
        {
            let lease = crate::exec::retain_fork_sensitive_descriptors();
            self.open_regular(name, writable)?
                .map(|file| {
                    crate::exec::InheritedDescriptorAuthority::from_owned_file(file, &lease)
                        .map_err(anyhow::Error::msg)
                })
                .transpose()
        }
    }

    /// Pin one direct child as an `O_PATH` mount source. Regular files,
    /// directories, and Unix sockets are accepted; links and special devices
    /// are rejected. The returned descriptor remains bound to the exact entry
    /// inode even if its ordinary pathname is later replaced.
    pub fn open_mount_entry(&self, name: &OsStr) -> Result<Option<File>> {
        #[cfg(not(target_os = "linux"))]
        {
            let _ = name;
            anyhow::bail!("descriptor-pinned mount entries are available only on Linux");
        }
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::fs::FileTypeExt as _;

            validate_child_name(name)?;
            let name_c = std::ffi::CString::new(name.as_bytes())?;
            let descriptor = unsafe {
                libc::openat(
                    self.directory.as_raw_fd(),
                    name_c.as_ptr(),
                    libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
            if descriptor < 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() == std::io::ErrorKind::NotFound {
                    return Ok(None);
                }
                return Err(error).with_context(|| {
                    format!("pin secure mount entry {}", self.path.join(name).display())
                });
            }
            let file = unsafe { File::from_raw_fd(descriptor) };
            let file_type = file.metadata()?.file_type();
            if !(file_type.is_file() || file_type.is_dir() || file_type.is_socket()) {
                anyhow::bail!(
                    "secure mount entry is not a regular file, directory, or Unix socket: {}",
                    self.path.join(name).display()
                );
            }
            Ok(Some(file))
        }
    }

    /// Create and publish one regular file without replacing an existing
    /// entry. A successful create returns the still-open exact inode; `None`
    /// means another entry already owns the target name.
    pub fn atomic_create_regular(
        &self,
        name: &OsStr,
        bytes: &[u8],
        mode: u32,
    ) -> Result<Option<File>> {
        #[cfg(not(unix))]
        {
            let _ = (name, bytes, mode);
            anyhow::bail!("secure atomic creation is unavailable on this platform");
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;

            validate_child_name(name)?;
            if self.open_regular(name, false)?.is_some() {
                return Ok(None);
            }
            let name_c = std::ffi::CString::new(name.as_bytes())?;
            let sequence = crate::atomic_fs::next_temp_sequence();
            let temp_name =
                std::ffi::CString::new(format!(".secure.tmp.{}.{}", std::process::id(), sequence))?;
            let descriptor = unsafe {
                libc::openat(
                    self.directory.as_raw_fd(),
                    temp_name.as_ptr(),
                    libc::O_RDWR
                        | libc::O_CREAT
                        | libc::O_EXCL
                        | libc::O_NOFOLLOW
                        | libc::O_CLOEXEC,
                    mode,
                )
            };
            if descriptor < 0 {
                return Err(std::io::Error::last_os_error())
                    .with_context(|| format!("create secure temp in {}", self.path.display()));
            }
            let mut temp = unsafe { File::from_raw_fd(descriptor) };
            let result = (|| -> Result<bool> {
                temp.write_all(bytes)?;
                temp.set_permissions(std::fs::Permissions::from_mode(mode))?;
                temp.sync_all()?;
                match publish_temp_without_replacement(
                    &self.directory,
                    &temp_name,
                    &name_c,
                    &self.path.join(name),
                ) {
                    Ok(()) => {
                        self.directory.sync_all()?;
                        Ok(true)
                    }
                    Err(error)
                        if self.open_regular(name, false)?.is_some()
                            && error.downcast_ref::<std::io::Error>().is_some_and(|error| {
                                error.kind() == std::io::ErrorKind::AlreadyExists
                            }) =>
                    {
                        Ok(false)
                    }
                    Err(error) => Err(error),
                }
            })();
            match result {
                Ok(true) => Ok(Some(temp)),
                Ok(false) => {
                    unsafe {
                        libc::unlinkat(self.directory.as_raw_fd(), temp_name.as_ptr(), 0);
                    }
                    Ok(None)
                }
                Err(error) => {
                    unsafe {
                        libc::unlinkat(self.directory.as_raw_fd(), temp_name.as_ptr(), 0);
                    }
                    Err(error)
                }
            }
        }
    }

    /// Typed counterpart to [`Self::atomic_create_regular`]. The returned
    /// child coordinate and exact created inode remain one Lillux authority.
    pub fn atomic_create_pinned_regular(
        &self,
        name: &OsStr,
        bytes: &[u8],
        mode: u32,
    ) -> Result<Option<PinnedRegularFile>> {
        Ok(self
            .atomic_create_regular(name, bytes, mode)?
            .map(|file| PinnedRegularFile {
                path: self.path.join(name),
                name: name.to_os_string(),
                file,
            }))
    }

    /// Stream and publish one bounded regular file without replacing an
    /// existing entry. The temporary inode is created relative to this pinned
    /// directory, fsynced, and renamed without replacement. A body exceeding
    /// `maximum_bytes` is rejected before namespace publication.
    pub fn atomic_create_regular_from_reader<R: Read>(
        &self,
        name: &OsStr,
        reader: &mut R,
        maximum_bytes: u64,
        mode: u32,
    ) -> Result<Option<(File, u64)>> {
        #[cfg(not(unix))]
        {
            let _ = (name, reader, maximum_bytes, mode);
            anyhow::bail!("secure streamed atomic creation is unavailable on this platform");
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;

            if maximum_bytes == 0 {
                anyhow::bail!("secure streamed atomic creation requires a positive byte bound");
            }
            validate_child_name(name)?;
            if self.open_regular(name, false)?.is_some() {
                return Ok(None);
            }
            let name_c = std::ffi::CString::new(name.as_bytes())?;
            let sequence = crate::atomic_fs::next_temp_sequence();
            let temp_name =
                std::ffi::CString::new(format!(".secure.tmp.{}.{}", std::process::id(), sequence))?;
            let descriptor = unsafe {
                libc::openat(
                    self.directory.as_raw_fd(),
                    temp_name.as_ptr(),
                    libc::O_RDWR
                        | libc::O_CREAT
                        | libc::O_EXCL
                        | libc::O_NOFOLLOW
                        | libc::O_CLOEXEC,
                    mode,
                )
            };
            if descriptor < 0 {
                return Err(std::io::Error::last_os_error()).with_context(|| {
                    format!("create secure streamed temp in {}", self.path.display())
                });
            }
            let mut temp = unsafe { File::from_raw_fd(descriptor) };
            let result = (|| -> Result<Option<u64>> {
                let sentinel_bound = maximum_bytes
                    .checked_add(1)
                    .ok_or_else(|| anyhow::anyhow!("streamed byte bound overflow"))?;
                let copied = std::io::copy(&mut reader.take(sentinel_bound), &mut temp)?;
                if copied > maximum_bytes {
                    anyhow::bail!(
                        "secure streamed regular file exceeds the {maximum_bytes}-byte bound"
                    );
                }
                temp.set_permissions(std::fs::Permissions::from_mode(mode))?;
                temp.sync_all()?;
                match publish_temp_without_replacement(
                    &self.directory,
                    &temp_name,
                    &name_c,
                    &self.path.join(name),
                ) {
                    Ok(()) => {
                        self.directory.sync_all()?;
                        Ok(Some(copied))
                    }
                    Err(error)
                        if self.open_regular(name, false)?.is_some()
                            && error.downcast_ref::<std::io::Error>().is_some_and(|error| {
                                error.kind() == std::io::ErrorKind::AlreadyExists
                            }) =>
                    {
                        Ok(None)
                    }
                    Err(error) => Err(error),
                }
            })();
            match result {
                Ok(Some(copied)) => Ok(Some((temp, copied))),
                Ok(None) => {
                    unsafe {
                        libc::unlinkat(self.directory.as_raw_fd(), temp_name.as_ptr(), 0);
                    }
                    Ok(None)
                }
                Err(error) => {
                    unsafe {
                        libc::unlinkat(self.directory.as_raw_fd(), temp_name.as_ptr(), 0);
                    }
                    Err(error)
                }
            }
        }
    }

    /// Stream and publish one bounded regular file while retaining its typed
    /// descriptor authority. This is the pinned counterpart to
    /// [`Self::atomic_create_regular_from_reader`]; callers that pass the
    /// result across an authority boundary do not need to erase the exact
    /// inode into a bare [`File`] or reopen its ambient pathname.
    pub fn atomic_create_pinned_regular_from_reader<R: Read>(
        &self,
        name: &OsStr,
        reader: &mut R,
        maximum_bytes: u64,
        mode: u32,
    ) -> Result<Option<(PinnedRegularFile, u64)>> {
        let Some((mut file, copied)) =
            self.atomic_create_regular_from_reader(name, reader, maximum_bytes, mode)?
        else {
            return Ok(None);
        };
        file.rewind()?;
        Ok(Some((
            PinnedRegularFile {
                path: self.path.join(name),
                name: name.to_os_string(),
                file,
            },
            copied,
        )))
    }

    /// Open or create one regular child while retaining this directory inode.
    pub fn open_regular_create(
        &self,
        name: &OsStr,
        writable: bool,
        create_new: bool,
        mode: u32,
    ) -> Result<File> {
        #[cfg(not(unix))]
        {
            let _ = (name, writable, create_new, mode);
            anyhow::bail!("secure regular-file creation is unavailable on this platform");
        }
        #[cfg(unix)]
        {
            validate_child_name(name)?;
            let name_c = std::ffi::CString::new(name.as_bytes())?;
            let create_flags = libc::O_CREAT | if create_new { libc::O_EXCL } else { 0 };
            open_regular_at_flags(
                &self.directory,
                &name_c,
                &self.path.join(name),
                if writable {
                    libc::O_RDWR
                } else {
                    libc::O_RDONLY
                },
                create_flags,
                mode,
            )?
            .ok_or_else(|| anyhow::anyhow!("created regular file disappeared"))
        }
    }

    /// Open or create one regular child as an exact typed authority.
    ///
    /// This is the create counterpart to [`Self::open_pinned_regular`]. It is
    /// intended for descriptor-scoped leases and locks whose OS mechanics
    /// must not escape into the caller.
    pub fn open_pinned_regular_create(
        &self,
        name: &OsStr,
        writable: bool,
        create_new: bool,
        mode: u32,
    ) -> Result<PinnedRegularFile> {
        let file = self.open_regular_create(name, writable, create_new, mode)?;
        Ok(PinnedRegularFile {
            path: self.path.join(name),
            name: name.to_os_string(),
            file,
        })
    }

    /// Materialize one already-open immutable regular file as a new child.
    ///
    /// A descriptor-to-descriptor reflink is attempted first. When that
    /// filesystem operation is unavailable, the caller-owned aggregate copy
    /// allowance is charged before any bytes are copied. The source and target
    /// identities are revalidated before return.
    pub fn materialize_private_regular_child(
        &self,
        name: &OsStr,
        source: &File,
        expected_size: u64,
        mode: u32,
        remaining_copy_bytes: &mut u64,
    ) -> Result<PrivateFileMaterialization> {
        let source_observation = observe_open_regular_file(source)?;
        if source_observation.size() != expected_size {
            anyhow::bail!(
                "private materialization source size {} differs from admitted size {expected_size}",
                source_observation.size()
            );
        }
        let mut target = self.open_regular_create(name, true, true, 0o600)?;
        let result = (|| {
            let outcome = if try_reflink_regular_file(source, &target)? {
                PrivateFileMaterialization::Reflink
            } else {
                if expected_size > *remaining_copy_bytes {
                    anyhow::bail!(
                        "private materialization fallback copy requires {expected_size} bytes but only {} bytes remain",
                        *remaining_copy_bytes
                    );
                }
                target.set_len(0)?;
                target.rewind()?;
                let mut source = source.try_clone()?;
                source.rewind()?;
                let copied = std::io::copy(
                    &mut source.take(expected_size.saturating_add(1)),
                    &mut target,
                )?;
                if copied != expected_size {
                    anyhow::bail!(
                        "private materialization source changed size while copying: expected {expected_size}, copied {copied}"
                    );
                }
                *remaining_copy_bytes -= expected_size;
                PrivateFileMaterialization::Copied
            };
            ensure_open_regular_file_unchanged(source, &source_observation)?;
            if target.metadata()?.len() != expected_size {
                anyhow::bail!("private materialization target has the wrong size");
            }
            set_open_regular_file_mode(&target, mode)?;
            self.ensure_regular_entry_matches(name, Some(&target))?;
            Ok(outcome)
        })();
        if result.is_err() {
            let _ = self.remove_if_same(name, &target);
        }
        result
    }

    /// Publish an already-open regular file from another pinned directory as a
    /// create-only hard link. This is used to promote a fully written CAS
    /// staging file after its content address is known. `false` means the
    /// target name already exists and must be verified by the caller.
    pub fn publish_regular_link_from(
        &self,
        target_name: &OsStr,
        source_directory: &PinnedDirectory,
        source_name: &OsStr,
        expected_source: &File,
    ) -> Result<bool> {
        #[cfg(not(unix))]
        {
            let _ = (target_name, source_directory, source_name, expected_source);
            anyhow::bail!("secure linked publication is unavailable on this platform")
        }
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt as _;

            validate_child_name(target_name)?;
            validate_child_name(source_name)?;
            let source_name_c = std::ffi::CString::new(source_name.as_bytes())?;
            let target_name_c = std::ffi::CString::new(target_name.as_bytes())?;
            ensure_entry_matches(
                &source_directory.directory,
                &source_name_c,
                Some(expected_source),
                &source_directory.path.join(source_name),
            )?;
            if unsafe {
                libc::linkat(
                    source_directory.directory.as_raw_fd(),
                    source_name_c.as_ptr(),
                    self.directory.as_raw_fd(),
                    target_name_c.as_ptr(),
                    0,
                )
            } != 0
            {
                let error = std::io::Error::last_os_error();
                if error.kind() == std::io::ErrorKind::AlreadyExists {
                    return Ok(false);
                }
                return Err(error).with_context(|| {
                    format!(
                        "publish secure staged file {} as {}",
                        source_directory.path.join(source_name).display(),
                        self.path.join(target_name).display()
                    )
                });
            }
            self.directory.sync_all()?;
            source_directory.remove_if_same(source_name, expected_source)?;
            Ok(true)
        }
    }

    /// Create a hard link from one exact named regular file in a pinned source
    /// directory under one validated child name of this exact directory inode.
    /// Both directory descriptors and the expected source file stay pinned for
    /// the entire operation, so neither side can be rebound by replacing an
    /// ambient pathname.
    ///
    /// An existing destination is accepted only when it is the same regular
    /// inode as `source`.
    pub fn link_regular_from(
        &self,
        target_name: &OsStr,
        source_directory: &PinnedDirectory,
        source_name: &OsStr,
        expected_source: &File,
    ) -> Result<()> {
        #[cfg(not(unix))]
        {
            let _ = (target_name, source_directory, source_name, expected_source);
            anyhow::bail!("secure hard linking is unavailable on this platform")
        }
        #[cfg(unix)]
        {
            validate_child_name(target_name)?;
            validate_child_name(source_name)?;
            let source_name_c = std::ffi::CString::new(source_name.as_bytes())?;
            let target_name_c = std::ffi::CString::new(target_name.as_bytes())?;
            ensure_entry_matches(
                &source_directory.directory,
                &source_name_c,
                Some(expected_source),
                &source_directory.path.join(source_name),
            )?;
            if unsafe {
                libc::linkat(
                    source_directory.directory.as_raw_fd(),
                    source_name_c.as_ptr(),
                    self.directory.as_raw_fd(),
                    target_name_c.as_ptr(),
                    0,
                )
            } != 0
            {
                let error = std::io::Error::last_os_error();
                if error.kind() != std::io::ErrorKind::AlreadyExists {
                    return Err(error).with_context(|| {
                        format!(
                            "link secure regular file {} into {}",
                            source_directory.path.join(source_name).display(),
                            self.path.join(target_name).display()
                        )
                    });
                }
                let existing = self.open_regular(target_name, false)?.ok_or_else(|| {
                    anyhow::anyhow!(
                        "existing hard-link destination is not a regular file: {}",
                        self.path.join(target_name).display()
                    )
                })?;
                use std::os::unix::fs::MetadataExt as _;
                let source_metadata = expected_source.metadata()?;
                let existing_metadata = existing.metadata()?;
                if !existing_metadata.file_type().is_file()
                    || source_metadata.dev() != existing_metadata.dev()
                    || source_metadata.ino() != existing_metadata.ino()
                {
                    anyhow::bail!(
                        "destination conflicts with pinned source inode: {}",
                        self.path.join(target_name).display()
                    );
                }
            }
            self.directory.sync_all()?;
            Ok(())
        }
    }

    /// Publish one exact pinned child directory under another name in this
    /// same parent without replacing an existing entry. The parent descriptor
    /// and expected source inode are retained across the whole operation.
    pub fn rename_child_directory_noreplace(
        &self,
        source_name: &OsStr,
        target_name: &OsStr,
        expected_source: &Self,
    ) -> crate::atomic_fs::AtomicMutationResult<()> {
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (source_name, target_name, expected_source);
            return Err(crate::atomic_fs::AtomicMutationError::before(
                anyhow::anyhow!(
                    "descriptor-relative no-replace rename is unavailable on this platform"
                ),
            ));
        }
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::fs::MetadataExt as _;

            validate_child_name(source_name)
                .map_err(crate::atomic_fs::AtomicMutationError::before)?;
            validate_child_name(target_name)
                .map_err(crate::atomic_fs::AtomicMutationError::before)?;
            let source_name_c = std::ffi::CString::new(source_name.as_bytes())
                .map_err(crate::atomic_fs::AtomicMutationError::before)?;
            let target_name_c = std::ffi::CString::new(target_name.as_bytes())
                .map_err(crate::atomic_fs::AtomicMutationError::before)?;
            let current = self
                .open_child_directory(source_name)
                .map_err(crate::atomic_fs::AtomicMutationError::before)?
                .ok_or_else(|| anyhow::anyhow!("rename source directory disappeared"))
                .map_err(crate::atomic_fs::AtomicMutationError::before)?;
            let current_metadata = current
                .directory
                .metadata()
                .map_err(crate::atomic_fs::AtomicMutationError::before)?;
            let expected_metadata = expected_source
                .directory
                .metadata()
                .map_err(crate::atomic_fs::AtomicMutationError::before)?;
            if current_metadata.dev() != expected_metadata.dev()
                || current_metadata.ino() != expected_metadata.ino()
            {
                return Err(crate::atomic_fs::AtomicMutationError::before(
                    anyhow::anyhow!(
                        "rename source directory changed before publication: {}",
                        self.path.join(source_name).display()
                    ),
                ));
            }
            if unsafe {
                libc::renameat2(
                    self.directory.as_raw_fd(),
                    source_name_c.as_ptr(),
                    self.directory.as_raw_fd(),
                    target_name_c.as_ptr(),
                    libc::RENAME_NOREPLACE,
                )
            } != 0
            {
                return Err(crate::atomic_fs::AtomicMutationError::before(
                    anyhow::Error::new(std::io::Error::last_os_error()).context(format!(
                        "publish pinned directory {} as {}",
                        self.path.join(source_name).display(),
                        self.path.join(target_name).display()
                    )),
                ));
            }
            self.directory
                .sync_all()
                .map_err(crate::atomic_fs::AtomicMutationError::durability)?;
            Ok(())
        }
    }

    /// Move one exact pinned regular child to a new name in the same directory
    /// without replacing any existing entry. Namespace publication is atomic;
    /// callers can therefore recover a crash by observing either source or
    /// destination, never a link-then-unlink intermediate state.
    pub fn rename_regular_child_noreplace_atomic(
        &self,
        source_name: &OsStr,
        target_name: &OsStr,
        expected_source: &File,
    ) -> crate::atomic_fs::AtomicMutationResult<()> {
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (source_name, target_name, expected_source);
            return Err(crate::atomic_fs::AtomicMutationError::before(
                anyhow::anyhow!(
                    "descriptor-relative no-replace regular rename is unavailable on this platform"
                ),
            ));
        }
        #[cfg(target_os = "linux")]
        {
            validate_child_name(source_name)
                .map_err(crate::atomic_fs::AtomicMutationError::before)?;
            validate_child_name(target_name)
                .map_err(crate::atomic_fs::AtomicMutationError::before)?;
            let source_name_c = std::ffi::CString::new(source_name.as_bytes())
                .map_err(crate::atomic_fs::AtomicMutationError::before)?;
            let target_name_c = std::ffi::CString::new(target_name.as_bytes())
                .map_err(crate::atomic_fs::AtomicMutationError::before)?;
            ensure_entry_matches(
                &self.directory,
                &source_name_c,
                Some(expected_source),
                &self.path.join(source_name),
            )
            .map_err(crate::atomic_fs::AtomicMutationError::before)?;
            if unsafe {
                libc::renameat2(
                    self.directory.as_raw_fd(),
                    source_name_c.as_ptr(),
                    self.directory.as_raw_fd(),
                    target_name_c.as_ptr(),
                    libc::RENAME_NOREPLACE,
                )
            } != 0
            {
                return Err(crate::atomic_fs::AtomicMutationError::before(
                    anyhow::Error::new(std::io::Error::last_os_error()).context(format!(
                        "preserve pinned regular file {} as {}",
                        self.path.join(source_name).display(),
                        self.path.join(target_name).display()
                    )),
                ));
            }
            self.directory
                .sync_all()
                .map_err(crate::atomic_fs::AtomicMutationError::durability)?;
            Ok(())
        }
    }

    /// Move one exact regular entry from `source_directory` into this pinned
    /// directory, atomically replacing an existing regular target. Both parent
    /// descriptors and the source inode remain pinned through the mutation.
    pub fn rename_regular_from(
        &self,
        target_name: &OsStr,
        source_directory: &PinnedDirectory,
        source_name: &OsStr,
        expected_source: &File,
    ) -> Result<()> {
        self.rename_regular_from_atomic(target_name, source_directory, source_name, expected_source)
            .map_err(Into::into)
    }

    /// Commit-aware form of [`Self::rename_regular_from`].
    pub fn rename_regular_from_atomic(
        &self,
        target_name: &OsStr,
        source_directory: &PinnedDirectory,
        source_name: &OsStr,
        expected_source: &File,
    ) -> crate::atomic_fs::AtomicMutationResult<()> {
        #[cfg(not(unix))]
        {
            let _ = (target_name, source_directory, source_name, expected_source);
            return Err(crate::atomic_fs::AtomicMutationError::before(
                anyhow::anyhow!(
                    "descriptor-relative regular rename is unavailable on this platform"
                ),
            ));
        }
        #[cfg(unix)]
        {
            validate_child_name(target_name)
                .map_err(crate::atomic_fs::AtomicMutationError::before)?;
            validate_child_name(source_name)
                .map_err(crate::atomic_fs::AtomicMutationError::before)?;
            let source_name_c = std::ffi::CString::new(source_name.as_bytes())
                .map_err(crate::atomic_fs::AtomicMutationError::before)?;
            let target_name_c = std::ffi::CString::new(target_name.as_bytes())
                .map_err(crate::atomic_fs::AtomicMutationError::before)?;
            ensure_entry_matches(
                &source_directory.directory,
                &source_name_c,
                Some(expected_source),
                &source_directory.path.join(source_name),
            )
            .map_err(crate::atomic_fs::AtomicMutationError::before)?;
            if matches!(
                self.open_entry(target_name, false)
                    .map_err(crate::atomic_fs::AtomicMutationError::before)?,
                Some(PinnedDirectoryEntry::Directory(_))
            ) {
                return Err(crate::atomic_fs::AtomicMutationError::before(
                    anyhow::anyhow!(
                        "regular rename target is a directory: {}",
                        self.path.join(target_name).display()
                    ),
                ));
            }
            if unsafe {
                libc::renameat(
                    source_directory.directory.as_raw_fd(),
                    source_name_c.as_ptr(),
                    self.directory.as_raw_fd(),
                    target_name_c.as_ptr(),
                )
            } != 0
            {
                return Err(crate::atomic_fs::AtomicMutationError::before(
                    anyhow::Error::new(std::io::Error::last_os_error()).context(format!(
                        "move pinned regular file {} to {}",
                        source_directory.path.join(source_name).display(),
                        self.path.join(target_name).display()
                    )),
                ));
            }
            source_directory
                .directory
                .sync_all()
                .map_err(crate::atomic_fs::AtomicMutationError::durability)?;
            if !self
                .is_same_directory(source_directory)
                .map_err(crate::atomic_fs::AtomicMutationError::durability)?
            {
                self.directory
                    .sync_all()
                    .map_err(crate::atomic_fs::AtomicMutationError::durability)?;
            }
            Ok(())
        }
    }

    /// Replace an expected regular target, or create an expected-absent
    /// target, without ever overwriting an unexpected namespace entry.
    /// Existing targets are first moved to a private quarantine name and
    /// verified by inode before the source is published with NOREPLACE.
    pub fn replace_regular_from_if_matches_atomic<V>(
        &self,
        target_name: &OsStr,
        expected_target: Option<&File>,
        validate_expected_target: V,
        source_directory: &PinnedDirectory,
        source_name: &OsStr,
        expected_source: &File,
    ) -> crate::atomic_fs::AtomicMutationResult<()>
    where
        V: FnOnce(&File) -> Result<()>,
    {
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (
                target_name,
                expected_target,
                validate_expected_target,
                source_directory,
                source_name,
                expected_source,
            );
            return Err(crate::atomic_fs::AtomicMutationError::before(
                anyhow::anyhow!("conditional regular replacement requires Linux renameat2"),
            ));
        }
        #[cfg(target_os = "linux")]
        {
            validate_child_name(target_name)
                .map_err(crate::atomic_fs::AtomicMutationError::before)?;
            validate_child_name(source_name)
                .map_err(crate::atomic_fs::AtomicMutationError::before)?;
            let target_name_c = std::ffi::CString::new(target_name.as_bytes())
                .map_err(crate::atomic_fs::AtomicMutationError::before)?;
            let source_name_c = std::ffi::CString::new(source_name.as_bytes())
                .map_err(crate::atomic_fs::AtomicMutationError::before)?;
            ensure_entry_matches(
                &source_directory.directory,
                &source_name_c,
                Some(expected_source),
                &source_directory.path.join(source_name),
            )
            .map_err(crate::atomic_fs::AtomicMutationError::before)?;

            let Some(expected_target) = expected_target else {
                rename_noreplace_between(
                    &source_directory.directory,
                    &source_name_c,
                    &self.directory,
                    &target_name_c,
                )
                .map_err(crate::atomic_fs::AtomicMutationError::before)?;
                source_directory
                    .directory
                    .sync_all()
                    .map_err(crate::atomic_fs::AtomicMutationError::durability)?;
                if !self
                    .is_same_directory(source_directory)
                    .map_err(crate::atomic_fs::AtomicMutationError::durability)?
                {
                    self.directory
                        .sync_all()
                        .map_err(crate::atomic_fs::AtomicMutationError::durability)?;
                }
                return Ok(());
            };

            let (quarantine_name, quarantine_name_c) =
                self.move_regular_to_unique_quarantine(&target_name_c)?;
            let identity_check = ensure_entry_matches(
                &self.directory,
                &quarantine_name_c,
                Some(expected_target),
                &self.path.join(&quarantine_name),
            );
            if let Err(error) = identity_check {
                return match restore_quarantined_regular(
                    &self.directory,
                    &quarantine_name_c,
                    &target_name_c,
                ) {
                    Ok(()) => Err(crate::atomic_fs::AtomicMutationError::before(
                        error.context("conditional replace target changed before commit"),
                    )),
                    Err(restore) => Err(crate::atomic_fs::AtomicMutationError::namespace_changed(
                        anyhow::anyhow!(
                            "conditional replace refused an unexpected target; it remains preserved as {} because restoration raced: {error:#}; {restore:#}",
                            self.path.join(&quarantine_name).display()
                        ),
                    )),
                };
            }
            if let Err(error) = validate_expected_target(expected_target) {
                return match restore_quarantined_regular(
                    &self.directory,
                    &quarantine_name_c,
                    &target_name_c,
                ) {
                    Ok(()) => Err(crate::atomic_fs::AtomicMutationError::before(
                        error.context("conditional replace target content changed before commit"),
                    )),
                    Err(restore) => Err(crate::atomic_fs::AtomicMutationError::namespace_changed(
                        anyhow::anyhow!(
                            "conditional replace refused changed target content; it remains preserved as {} because restoration raced: {error:#}; {restore:#}",
                            self.path.join(&quarantine_name).display()
                        ),
                    )),
                };
            }

            if let Err(error) = rename_noreplace_between(
                &source_directory.directory,
                &source_name_c,
                &self.directory,
                &target_name_c,
            ) {
                return match restore_quarantined_regular(
                    &self.directory,
                    &quarantine_name_c,
                    &target_name_c,
                ) {
                    Ok(()) => Err(crate::atomic_fs::AtomicMutationError::before(
                        anyhow::Error::new(error)
                            .context("conditional replace target was occupied before publication"),
                    )),
                    Err(restore) => Err(crate::atomic_fs::AtomicMutationError::namespace_changed(
                        anyhow::anyhow!(
                            "conditional replace did not publish; the verified prior target remains preserved as {} because restoration raced: {error:#}; {restore:#}",
                            self.path.join(&quarantine_name).display()
                        ),
                    )),
                };
            }
            if unsafe { libc::unlinkat(self.directory.as_raw_fd(), quarantine_name_c.as_ptr(), 0) }
                != 0
            {
                return Err(crate::atomic_fs::AtomicMutationError::durability(
                    anyhow::Error::new(std::io::Error::last_os_error()).context(format!(
                        "remove replaced target quarantine {}",
                        self.path.join(quarantine_name).display()
                    )),
                ));
            }
            source_directory
                .directory
                .sync_all()
                .map_err(crate::atomic_fs::AtomicMutationError::durability)?;
            if !self
                .is_same_directory(source_directory)
                .map_err(crate::atomic_fs::AtomicMutationError::durability)?
            {
                self.directory
                    .sync_all()
                    .map_err(crate::atomic_fs::AtomicMutationError::durability)?;
            }
            Ok(())
        }
    }

    #[cfg(target_os = "linux")]
    fn move_regular_to_unique_quarantine(
        &self,
        source_name: &std::ffi::CStr,
    ) -> crate::atomic_fs::AtomicMutationResult<(OsString, std::ffi::CString)> {
        for _ in 0..16 {
            let name = OsString::from(format!(
                ".ryeos-quarantine.{}.{}",
                std::process::id(),
                crate::atomic_fs::next_temp_sequence()
            ));
            let name_c = std::ffi::CString::new(name.as_bytes())
                .map_err(crate::atomic_fs::AtomicMutationError::before)?;
            match rename_noreplace_between(&self.directory, source_name, &self.directory, &name_c) {
                Ok(()) => return Ok((name, name_c)),
                Err(error) if error.raw_os_error() == Some(libc::EEXIST) => continue,
                Err(error) => {
                    return Err(crate::atomic_fs::AtomicMutationError::before(error));
                }
            }
        }
        Err(crate::atomic_fs::AtomicMutationError::before(
            anyhow::anyhow!("could not reserve a unique regular-file quarantine name"),
        ))
    }

    /// Enumerate a strict flat namespace and return open handles for every
    /// regular entry. Directories, links, sockets, and devices are errors.
    pub fn regular_files(&self) -> Result<Vec<PinnedRegularFile>> {
        self.regular_files_with_limit(None)
    }

    /// Bounded strict flat regular-file enumeration.
    pub fn regular_files_bounded(&self, max_entries: usize) -> Result<Vec<PinnedRegularFile>> {
        self.regular_files_with_limit(Some(max_entries))
    }

    fn regular_files_with_limit(
        &self,
        max_entries: Option<usize>,
    ) -> Result<Vec<PinnedRegularFile>> {
        #[cfg(not(unix))]
        anyhow::bail!("secure directory enumeration is unavailable on this platform");
        #[cfg(unix)]
        {
            let mut entries = Vec::new();
            let names = match max_entries {
                Some(max_entries) => directory_names_with_limit(&self.directory, max_entries)?,
                None => directory_names(&self.directory)?,
            };
            for name in names {
                validate_child_name(&name)?;
                let name_c = std::ffi::CString::new(name.as_bytes())?;
                let path = self.path.join(&name);
                let file = open_regular_at(&self.directory, &name_c, &path)?
                    .ok_or_else(|| anyhow::anyhow!("secure directory entry disappeared"))?;
                entries.push(PinnedRegularFile { path, name, file });
            }
            Ok(entries)
        }
    }

    /// Atomically publish bytes relative to this pinned directory. `expected`
    /// binds replacement to the exact previously-opened inode; `None` requires
    /// the name to remain absent.
    pub fn atomic_write_if_same(
        &self,
        name: &OsStr,
        expected: Option<&File>,
        bytes: &[u8],
        mode: u32,
    ) -> Result<()> {
        #[cfg(not(unix))]
        {
            let _ = (name, expected, bytes, mode);
            anyhow::bail!("secure atomic publication is unavailable on this platform");
        }
        #[cfg(unix)]
        {
            validate_child_name(name)?;
            let name_c = std::ffi::CString::new(name.as_bytes())?;
            let sequence = crate::atomic_fs::next_temp_sequence();
            let temp_name =
                std::ffi::CString::new(format!(".secure.tmp.{}.{}", std::process::id(), sequence))?;
            let descriptor = unsafe {
                libc::openat(
                    self.directory.as_raw_fd(),
                    temp_name.as_ptr(),
                    libc::O_WRONLY
                        | libc::O_CREAT
                        | libc::O_EXCL
                        | libc::O_NOFOLLOW
                        | libc::O_CLOEXEC,
                    mode,
                )
            };
            if descriptor < 0 {
                return Err(std::io::Error::last_os_error())
                    .with_context(|| format!("create secure temp in {}", self.path.display()));
            }
            let mut temp = unsafe { File::from_raw_fd(descriptor) };
            let result = (|| -> Result<()> {
                if unsafe { libc::fchmod(temp.as_raw_fd(), mode) } != 0 {
                    return Err(std::io::Error::last_os_error())
                        .context("set exact secure temporary-file permissions");
                }
                temp.write_all(bytes)?;
                temp.sync_all()?;
                match expected {
                    None => publish_temp_without_replacement(
                        &self.directory,
                        &temp_name,
                        &name_c,
                        &self.path.join(name),
                    )?,
                    Some(expected) => {
                        ensure_entry_matches(
                            &self.directory,
                            &name_c,
                            Some(expected),
                            &self.path.join(name),
                        )?;
                        if unsafe {
                            libc::renameat(
                                self.directory.as_raw_fd(),
                                temp_name.as_ptr(),
                                self.directory.as_raw_fd(),
                                name_c.as_ptr(),
                            )
                        } != 0
                        {
                            return Err(std::io::Error::last_os_error()).with_context(|| {
                                format!("publish secure file {}", self.path.join(name).display())
                            });
                        }
                    }
                }
                self.directory.sync_all()?;
                Ok(())
            })();
            if result.is_err() {
                unsafe {
                    libc::unlinkat(self.directory.as_raw_fd(), temp_name.as_ptr(), 0);
                }
            }
            result
        }
    }

    /// Typed counterpart to [`Self::atomic_write_if_same`]. The incumbent's
    /// raw descriptor never leaves Lillux.
    pub fn atomic_write_pinned_if_same(
        &self,
        name: &OsStr,
        expected: Option<&PinnedRegularFile>,
        bytes: &[u8],
        mode: u32,
    ) -> Result<()> {
        self.atomic_write_if_same(name, expected.map(|entry| &entry.file), bytes, mode)
    }

    /// Stage complete replacement bytes inside this exact directory, then
    /// publish them through the quarantine/NOREPLACE conditional boundary.
    /// The validation closure runs against the quarantined incumbent at the
    /// namespace linearization point. A durable same-directory recovery
    /// record makes a process or power loss during the quarantine interval
    /// recover to the exact old or new value on the next authoring attempt.
    /// Failures retain the typed atomic phase.
    pub fn replace_bytes_if_matches_atomic<V>(
        &self,
        name: &OsStr,
        expected: Option<&File>,
        validate_expected: V,
        bytes: &[u8],
        mode: u32,
    ) -> crate::atomic_fs::AtomicMutationResult<()>
    where
        V: FnOnce(&File) -> Result<()>,
    {
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (name, expected, validate_expected, bytes, mode);
            return Err(crate::atomic_fs::AtomicMutationError::before(
                anyhow::anyhow!("conditional byte replacement requires Linux renameat2"),
            ));
        }
        #[cfg(target_os = "linux")]
        {
            validate_child_name(name).map_err(crate::atomic_fs::AtomicMutationError::before)?;
            if let Err(error) = self.recover_conditional_byte_replacement(name) {
                return Err(crate::atomic_fs::AtomicMutationError::before(
                    anyhow::anyhow!(
                        "a prior conditional authoring transaction requires recovery before this replacement can begin: {error}"
                    ),
                ));
            }
            let sequence = crate::atomic_fs::next_temp_sequence();
            let temp_name = OsString::from(format!(
                ".secure.authoring.{}.{}",
                std::process::id(),
                sequence
            ));
            let temp_name_c = std::ffi::CString::new(temp_name.as_bytes())
                .map_err(crate::atomic_fs::AtomicMutationError::before)?;
            let descriptor = unsafe {
                libc::openat(
                    self.directory.as_raw_fd(),
                    temp_name_c.as_ptr(),
                    libc::O_WRONLY
                        | libc::O_CREAT
                        | libc::O_EXCL
                        | libc::O_NOFOLLOW
                        | libc::O_CLOEXEC,
                    0o600,
                )
            };
            if descriptor < 0 {
                return Err(crate::atomic_fs::AtomicMutationError::before(
                    anyhow::Error::new(std::io::Error::last_os_error())
                        .context("create conditional authoring stage"),
                ));
            }
            let mut staged = unsafe { File::from_raw_fd(descriptor) };
            let prepare = (|| -> Result<()> {
                if unsafe { libc::fchmod(staged.as_raw_fd(), mode) } != 0 {
                    return Err(std::io::Error::last_os_error())
                        .context("set exact authoring-stage permissions");
                }
                staged.write_all(bytes)?;
                staged.sync_all()?;
                Ok(())
            })();
            if let Err(error) = prepare {
                unsafe {
                    libc::unlinkat(self.directory.as_raw_fd(), temp_name_c.as_ptr(), 0);
                }
                return Err(crate::atomic_fs::AtomicMutationError::before(error));
            }
            let result = match expected {
                None => self.replace_regular_from_if_matches_atomic(
                    name,
                    None,
                    validate_expected,
                    self,
                    &temp_name,
                    &staged,
                ),
                Some(expected) => self.replace_staged_bytes_with_recovery(
                    name,
                    expected,
                    validate_expected,
                    &temp_name,
                    &staged,
                ),
            };
            if result.is_err() {
                unsafe {
                    libc::unlinkat(self.directory.as_raw_fd(), temp_name_c.as_ptr(), 0);
                }
            }
            result
        }
    }

    /// Typed counterpart to [`Self::replace_bytes_if_matches_atomic`] that
    /// keeps ordinary OS file descriptors inside Lillux.
    pub fn replace_pinned_bytes_if_matches_atomic<V>(
        &self,
        name: &OsStr,
        expected: Option<&PinnedRegularFile>,
        validate_expected: V,
        bytes: &[u8],
        mode: u32,
    ) -> crate::atomic_fs::AtomicMutationResult<()>
    where
        V: FnOnce(&PinnedRegularFile) -> Result<()>,
    {
        let diagnostic_path = self.path.join(name);
        let diagnostic_name = name.to_os_string();
        self.replace_bytes_if_matches_atomic(
            name,
            expected.map(|file| &file.file),
            move |file| {
                let pinned = PinnedRegularFile {
                    path: diagnostic_path,
                    name: diagnostic_name,
                    file: file.try_clone()?,
                };
                validate_expected(&pinned)
            },
            bytes,
            mode,
        )
    }

    /// Recover an interrupted conditional byte replacement for one exact child
    /// name without beginning another mutation. Higher-level durable jobs call
    /// this before strict namespace classification so the Lillux-owned recovery
    /// marker is never mistaken for workload state after a process or power
    /// loss.
    pub fn recover_conditional_byte_replacement_atomic(
        &self,
        target_name: &OsStr,
    ) -> crate::atomic_fs::AtomicMutationResult<()> {
        #[cfg(not(target_os = "linux"))]
        {
            let _ = target_name;
            Err(crate::atomic_fs::AtomicMutationError::before(
                anyhow::anyhow!("conditional byte replacement requires Linux renameat2"),
            ))
        }
        #[cfg(target_os = "linux")]
        {
            validate_child_name(target_name)
                .map_err(crate::atomic_fs::AtomicMutationError::before)?;
            self.recover_conditional_byte_replacement(target_name)
        }
    }

    #[cfg(target_os = "linux")]
    fn conditional_byte_recovery_name(name: &OsStr) -> OsString {
        use std::os::unix::ffi::OsStrExt as _;
        let digest = crate::sha256_hex(name.as_bytes());
        OsString::from(format!(".secure.authoring.recovery.{}", &digest[..32]))
    }

    #[cfg(target_os = "linux")]
    fn regular_identity_at(&self, name: &OsStr) -> Result<Option<RegularFileIdentity>> {
        let Some(entry) = self.entry_no_follow(name)? else {
            return Ok(None);
        };
        if entry.entry_type != PinnedEntryType::Regular {
            anyhow::bail!("conditional authoring recovery encountered a non-regular entry");
        }
        Ok(Some(RegularFileIdentity {
            containing_device: entry.containing_device,
            inode: entry.inode,
        }))
    }

    #[cfg(target_os = "linux")]
    fn unlink_regular_identity(&self, name: &OsStr, expected: RegularFileIdentity) -> Result<()> {
        use std::os::unix::ffi::OsStrExt as _;
        if self.regular_identity_at(name)? != Some(expected) {
            anyhow::bail!("conditional authoring recovery entry changed identity");
        }
        let name = std::ffi::CString::new(name.as_bytes())?;
        if unsafe { libc::unlinkat(self.directory.as_raw_fd(), name.as_ptr(), 0) } != 0 {
            return Err(std::io::Error::last_os_error())
                .context("remove exact conditional authoring recovery entry");
        }
        Ok(())
    }

    #[cfg(target_os = "linux")]
    fn recover_conditional_byte_replacement(
        &self,
        target_name: &OsStr,
    ) -> crate::atomic_fs::AtomicMutationResult<()> {
        use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};

        let marker_name = Self::conditional_byte_recovery_name(target_name);
        let Some(mut marker) = self
            .open_regular(&marker_name, false)
            .map_err(crate::atomic_fs::AtomicMutationError::namespace_changed)?
        else {
            return Ok(());
        };
        let marker_observation = observe_open_regular_file(&marker)
            .map_err(crate::atomic_fs::AtomicMutationError::namespace_changed)?;
        let marker_identity = regular_file_identity(&marker)
            .map_err(crate::atomic_fs::AtomicMutationError::namespace_changed)?;
        let bytes =
            read_open_regular_file_stable_bounded(&mut marker, &marker_observation, 16 * 1024)
                .map_err(crate::atomic_fs::AtomicMutationError::namespace_changed)?;
        let recovery: ConditionalByteReplacementRecovery = serde_json::from_slice(&bytes)
            .context("decode conditional authoring recovery record")
            .map_err(crate::atomic_fs::AtomicMutationError::namespace_changed)?;
        if recovery.schema != 1 || recovery.target_name.as_slice() != target_name.as_bytes() {
            return Err(crate::atomic_fs::AtomicMutationError::namespace_changed(
                anyhow::anyhow!("conditional authoring recovery record is not canonical"),
            ));
        }
        let stage_name = OsString::from_vec(recovery.stage_name.clone());
        let quarantine_name = OsString::from_vec(recovery.quarantine_name.clone());
        let target = self
            .regular_identity_at(target_name)
            .map_err(crate::atomic_fs::AtomicMutationError::namespace_changed)?;
        let stage = self
            .regular_identity_at(&stage_name)
            .map_err(crate::atomic_fs::AtomicMutationError::namespace_changed)?;
        let quarantine = self
            .regular_identity_at(&quarantine_name)
            .map_err(crate::atomic_fs::AtomicMutationError::namespace_changed)?;

        match (target, stage, quarantine) {
            (Some(target), _, Some(old))
                if target == recovery.staged_target && old == recovery.expected_target =>
            {
                self.unlink_regular_identity(&quarantine_name, old)
                    .map_err(crate::atomic_fs::AtomicMutationError::durability)?;
            }
            (Some(target), _, None) if target == recovery.staged_target => {}
            (Some(target), Some(stage), None)
                if target == recovery.expected_target && stage == recovery.staged_target =>
            {
                self.unlink_regular_identity(&stage_name, stage)
                    .map_err(crate::atomic_fs::AtomicMutationError::namespace_changed)?;
            }
            (Some(target), None, None) if target == recovery.expected_target => {}
            (None, Some(stage), Some(old))
                if stage == recovery.staged_target && old == recovery.expected_target =>
            {
                let quarantine_c = std::ffi::CString::new(recovery.quarantine_name)
                    .map_err(crate::atomic_fs::AtomicMutationError::namespace_changed)?;
                let target_c = std::ffi::CString::new(recovery.target_name)
                    .map_err(crate::atomic_fs::AtomicMutationError::namespace_changed)?;
                rename_noreplace_between(
                    &self.directory,
                    &quarantine_c,
                    &self.directory,
                    &target_c,
                )
                .map_err(|error| {
                    crate::atomic_fs::AtomicMutationError::namespace_changed(
                        anyhow::Error::new(error)
                            .context("restore interrupted conditional authoring target"),
                    )
                })?;
                self.unlink_regular_identity(&stage_name, stage)
                    .map_err(crate::atomic_fs::AtomicMutationError::namespace_changed)?;
            }
            _ => {
                return Err(crate::atomic_fs::AtomicMutationError::namespace_changed(
                    anyhow::anyhow!(
                        "conditional authoring recovery found an ambiguous target/stage/quarantine state"
                    ),
                ));
            }
        }
        self.unlink_regular_identity(&marker_name, marker_identity)
            .map_err(crate::atomic_fs::AtomicMutationError::durability)?;
        self.directory
            .sync_all()
            .map_err(crate::atomic_fs::AtomicMutationError::durability)
    }

    #[cfg(target_os = "linux")]
    fn replace_staged_bytes_with_recovery<V>(
        &self,
        target_name: &OsStr,
        expected_target: &File,
        validate_expected_target: V,
        stage_name: &OsStr,
        staged: &File,
    ) -> crate::atomic_fs::AtomicMutationResult<()>
    where
        V: FnOnce(&File) -> Result<()>,
    {
        use std::os::unix::ffi::OsStrExt as _;

        let expected_identity = regular_file_identity(expected_target)
            .map_err(crate::atomic_fs::AtomicMutationError::before)?;
        let staged_identity =
            regular_file_identity(staged).map_err(crate::atomic_fs::AtomicMutationError::before)?;
        let target_name_c = std::ffi::CString::new(target_name.as_bytes())
            .map_err(crate::atomic_fs::AtomicMutationError::before)?;
        let stage_name_c = std::ffi::CString::new(stage_name.as_bytes())
            .map_err(crate::atomic_fs::AtomicMutationError::before)?;
        ensure_entry_matches(
            &self.directory,
            &stage_name_c,
            Some(staged),
            &self.path.join(stage_name),
        )
        .map_err(crate::atomic_fs::AtomicMutationError::before)?;
        ensure_entry_matches(
            &self.directory,
            &target_name_c,
            Some(expected_target),
            &self.path.join(target_name),
        )
        .map_err(crate::atomic_fs::AtomicMutationError::before)?;

        let mut reserved_quarantine = None;
        for _ in 0..16 {
            let name = OsString::from(format!(
                ".secure.authoring.quarantine.{}.{}",
                std::process::id(),
                crate::atomic_fs::next_temp_sequence()
            ));
            if self
                .entry_no_follow(&name)
                .map_err(crate::atomic_fs::AtomicMutationError::before)?
                .is_none()
            {
                let name_c = std::ffi::CString::new(name.as_bytes())
                    .map_err(crate::atomic_fs::AtomicMutationError::before)?;
                reserved_quarantine = Some((name, name_c));
                break;
            }
        }
        let (quarantine_name, quarantine_name_c) = reserved_quarantine.ok_or_else(|| {
            crate::atomic_fs::AtomicMutationError::before(anyhow::anyhow!(
                "could not reserve conditional authoring quarantine name"
            ))
        })?;
        let marker_name = Self::conditional_byte_recovery_name(target_name);
        let recovery = ConditionalByteReplacementRecovery {
            schema: 1,
            target_name: target_name.as_bytes().to_vec(),
            stage_name: stage_name.as_bytes().to_vec(),
            quarantine_name: quarantine_name.as_bytes().to_vec(),
            expected_target: expected_identity,
            staged_target: staged_identity,
        };
        let recovery_bytes =
            serde_json::to_vec(&recovery).map_err(crate::atomic_fs::AtomicMutationError::before)?;
        self.atomic_write_if_same(&marker_name, None, &recovery_bytes, 0o600)
            .map_err(crate::atomic_fs::AtomicMutationError::before)?;

        let fail_before =
            |error: anyhow::Error| match self.recover_conditional_byte_replacement(target_name) {
                Ok(()) => crate::atomic_fs::AtomicMutationError::before(error),
                Err(recovery) => crate::atomic_fs::AtomicMutationError::namespace_changed(
                    anyhow::anyhow!("{error:#}; recovery failed: {recovery:#}"),
                ),
            };
        if let Err(error) = rename_noreplace_between(
            &self.directory,
            &target_name_c,
            &self.directory,
            &quarantine_name_c,
        ) {
            return Err(fail_before(
                anyhow::Error::new(error).context("quarantine conditional authoring target"),
            ));
        }
        if self
            .regular_identity_at(&quarantine_name)
            .map_err(crate::atomic_fs::AtomicMutationError::namespace_changed)?
            != Some(expected_identity)
        {
            return Err(fail_before(anyhow::anyhow!(
                "conditional authoring target changed before commit"
            )));
        }
        if let Err(error) = validate_expected_target(expected_target) {
            return Err(fail_before(error.context(
                "conditional authoring target content changed before commit",
            )));
        }
        if let Err(error) = rename_noreplace_between(
            &self.directory,
            &stage_name_c,
            &self.directory,
            &target_name_c,
        ) {
            return Err(fail_before(
                anyhow::Error::new(error).context("publish conditional authoring target"),
            ));
        }
        self.directory
            .sync_all()
            .map_err(crate::atomic_fs::AtomicMutationError::durability)?;
        self.unlink_regular_identity(&quarantine_name, expected_identity)
            .map_err(crate::atomic_fs::AtomicMutationError::durability)?;
        let marker = self
            .open_regular(&marker_name, false)
            .map_err(crate::atomic_fs::AtomicMutationError::durability)?
            .ok_or_else(|| {
                crate::atomic_fs::AtomicMutationError::durability(anyhow::anyhow!(
                    "conditional authoring recovery marker disappeared after commit"
                ))
            })?;
        let marker_identity = regular_file_identity(&marker)
            .map_err(crate::atomic_fs::AtomicMutationError::durability)?;
        self.unlink_regular_identity(&marker_name, marker_identity)
            .map_err(crate::atomic_fs::AtomicMutationError::durability)?;
        self.directory
            .sync_all()
            .map_err(crate::atomic_fs::AtomicMutationError::durability)
    }

    /// Write a complete hidden regular file for a later batch durability
    /// barrier and create-only publication. `None` means the target name
    /// already exists; the caller must verify that entry's exact bytes.
    pub(crate) fn prepare_atomic_create(
        &self,
        name: &OsStr,
        bytes: &[u8],
        mode: u32,
    ) -> Result<Option<PreparedAtomicCreate>> {
        #[cfg(not(unix))]
        {
            let _ = (name, bytes, mode);
            anyhow::bail!("secure prepared publication is unavailable on this platform");
        }
        #[cfg(unix)]
        {
            validate_child_name(name)?;
            if self.open_regular(name, false)?.is_some() {
                return Ok(None);
            }
            let directory = self.try_clone()?;
            let name_c = std::ffi::CString::new(name.as_bytes())?;
            let sequence = crate::atomic_fs::next_temp_sequence();
            let temp_name =
                std::ffi::CString::new(format!(".secure.tmp.{}.{}", std::process::id(), sequence))?;
            let descriptor = unsafe {
                libc::openat(
                    self.directory.as_raw_fd(),
                    temp_name.as_ptr(),
                    libc::O_WRONLY
                        | libc::O_CREAT
                        | libc::O_EXCL
                        | libc::O_NOFOLLOW
                        | libc::O_CLOEXEC,
                    mode,
                )
            };
            if descriptor < 0 {
                return Err(std::io::Error::last_os_error()).with_context(|| {
                    format!("create secure batch temp in {}", self.path.display())
                });
            }
            let mut temp = unsafe { File::from_raw_fd(descriptor) };
            if let Err(error) = temp.write_all(bytes) {
                unsafe {
                    libc::unlinkat(self.directory.as_raw_fd(), temp_name.as_ptr(), 0);
                }
                return Err(error).context("write secure batch temp");
            }
            Ok(Some(PreparedAtomicCreate {
                directory,
                temp_name,
                target_name: name_c,
                target_path: self.path.join(name),
                _temp_file: temp,
                published: false,
            }))
        }
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn sync_filesystem(&self) -> Result<()> {
        if unsafe { libc::syncfs(self.directory.as_raw_fd()) } != 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("sync filesystem for {}", self.path.display()));
        }
        Ok(())
    }

    #[cfg(unix)]
    pub(crate) fn filesystem_device(&self) -> Result<u64> {
        use std::os::unix::fs::MetadataExt;
        Ok(self.directory.metadata()?.dev())
    }

    /// Remove an exact previously-opened regular child and sync its directory.
    pub fn remove_if_same(&self, name: &OsStr, expected: &File) -> Result<()> {
        self.remove_if_same_atomic(name, expected)
            .map_err(Into::into)
    }

    /// Remove the child represented by one complete pinned-file authority.
    ///
    /// Keeping the coordinate and expected inode inside the same typed object
    /// prevents higher layers from accidentally authorizing removal with a
    /// name obtained from one lookup and a descriptor obtained from another.
    pub fn remove_pinned_regular_if_same(&self, expected: &PinnedRegularFile) -> Result<()> {
        self.remove_if_same(expected.name(), &expected.file)
    }

    /// Commit-aware form of [`Self::remove_if_same`].
    pub fn remove_if_same_atomic(
        &self,
        name: &OsStr,
        expected: &File,
    ) -> crate::atomic_fs::AtomicMutationResult<()> {
        self.remove_if_same_validated_atomic(name, expected, |_| Ok(()))
    }

    /// Conditionally remove an inode after a caller-supplied content/policy
    /// check at the quarantine linearization boundary.
    pub fn remove_if_same_validated_atomic<V>(
        &self,
        name: &OsStr,
        expected: &File,
        validate_expected: V,
    ) -> crate::atomic_fs::AtomicMutationResult<()>
    where
        V: FnOnce(&File) -> Result<()>,
    {
        #[cfg(not(unix))]
        {
            let _ = (name, expected);
            return Err(crate::atomic_fs::AtomicMutationError::before(
                anyhow::anyhow!("secure file removal is unavailable on this platform"),
            ));
        }
        #[cfg(unix)]
        {
            validate_child_name(name).map_err(crate::atomic_fs::AtomicMutationError::before)?;
            let name_c = std::ffi::CString::new(name.as_bytes())
                .map_err(crate::atomic_fs::AtomicMutationError::before)?;
            #[cfg(not(target_os = "linux"))]
            return Err(crate::atomic_fs::AtomicMutationError::before(
                anyhow::anyhow!("conditional regular removal requires Linux renameat2"),
            ));
            #[cfg(target_os = "linux")]
            let (quarantine_name, quarantine_name_c) =
                self.move_regular_to_unique_quarantine(&name_c)?;
            #[cfg(target_os = "linux")]
            if let Err(error) = ensure_entry_matches(
                &self.directory,
                &quarantine_name_c,
                Some(expected),
                &self.path.join(&quarantine_name),
            ) {
                return match restore_quarantined_regular(
                    &self.directory,
                    &quarantine_name_c,
                    &name_c,
                ) {
                    Ok(()) => Err(crate::atomic_fs::AtomicMutationError::before(
                        error.context("conditional remove target changed before commit"),
                    )),
                    Err(restore) => Err(crate::atomic_fs::AtomicMutationError::namespace_changed(
                        anyhow::anyhow!(
                            "conditional remove refused an unexpected target; it remains preserved as {} because restoration raced: {error:#}; {restore:#}",
                            self.path.join(&quarantine_name).display()
                        ),
                    )),
                };
            }
            #[cfg(target_os = "linux")]
            if let Err(error) = validate_expected(expected) {
                return match restore_quarantined_regular(
                    &self.directory,
                    &quarantine_name_c,
                    &name_c,
                ) {
                    Ok(()) => Err(crate::atomic_fs::AtomicMutationError::before(
                        error.context("conditional remove target content changed before commit"),
                    )),
                    Err(restore) => Err(crate::atomic_fs::AtomicMutationError::namespace_changed(
                        anyhow::anyhow!(
                            "conditional remove refused changed target content; it remains preserved as {} because restoration raced: {error:#}; {restore:#}",
                            self.path.join(&quarantine_name).display()
                        ),
                    )),
                };
            }
            #[cfg(target_os = "linux")]
            if unsafe { libc::unlinkat(self.directory.as_raw_fd(), quarantine_name_c.as_ptr(), 0) }
                != 0
            {
                return Err(crate::atomic_fs::AtomicMutationError::durability(
                    anyhow::Error::new(std::io::Error::last_os_error()).context(format!(
                        "remove secure file quarantine {}",
                        self.path.join(quarantine_name).display()
                    )),
                ));
            }
            self.directory
                .sync_all()
                .map_err(crate::atomic_fs::AtomicMutationError::durability)?;
            Ok(())
        }
    }

    /// Revalidate an expected regular child identity (or expected absence)
    /// immediately before a later descriptor-relative mutation.
    pub fn ensure_regular_entry_matches(
        &self,
        name: &OsStr,
        expected: Option<&File>,
    ) -> Result<()> {
        #[cfg(not(unix))]
        {
            let _ = (name, expected);
            anyhow::bail!("secure regular identity validation is unavailable on this platform")
        }
        #[cfg(unix)]
        {
            validate_child_name(name)?;
            let name_c = std::ffi::CString::new(name.as_bytes())?;
            ensure_entry_matches(&self.directory, &name_c, expected, &self.path.join(name))
        }
    }

    /// Remove an exact previously-opened empty child directory and sync its
    /// parent. A non-empty child is left in place and reported as `false`.
    pub fn remove_empty_child_if_same(&self, name: &OsStr, expected: &Self) -> Result<bool> {
        #[cfg(not(unix))]
        {
            let _ = (name, expected);
            anyhow::bail!("secure directory removal is unavailable on this platform");
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;

            validate_child_name(name)?;
            let name_c = std::ffi::CString::new(name.as_bytes())?;
            let path = self.path.join(name);
            #[cfg(target_os = "linux")]
            let current = self.open_mount_entry(name)?.ok_or_else(|| {
                anyhow::anyhow!("secure child directory disappeared: {}", path.display())
            })?;
            #[cfg(not(target_os = "linux"))]
            let current =
                open_child_directory(&self.directory, &name_c, &path)?.ok_or_else(|| {
                    anyhow::anyhow!("secure child directory disappeared: {}", path.display())
                })?;
            let current_metadata = current.metadata()?;
            let expected_metadata = expected.directory.metadata()?;
            if !current_metadata.is_dir()
                || current_metadata.dev() != expected_metadata.dev()
                || current_metadata.ino() != expected_metadata.ino()
            {
                anyhow::bail!(
                    "secure child directory changed before mutation: {}",
                    path.display()
                );
            }
            if unsafe {
                libc::unlinkat(
                    self.directory.as_raw_fd(),
                    name_c.as_ptr(),
                    libc::AT_REMOVEDIR,
                )
            } != 0
            {
                let error = std::io::Error::last_os_error();
                if matches!(
                    error.raw_os_error(),
                    Some(libc::ENOTEMPTY) | Some(libc::EEXIST)
                ) {
                    return Ok(false);
                }
                return Err(error)
                    .with_context(|| format!("remove secure directory {}", path.display()));
            }
            self.directory.sync_all()?;
            Ok(true)
        }
    }

    pub fn sync(&self) -> Result<()> {
        self.directory.sync_all()?;
        Ok(())
    }

    /// Durably sync every regular file and directory beneath this exact pinned
    /// root. Traversal remains descriptor-relative throughout; symlinks,
    /// special files, and disappearing entries fail closed.
    pub fn sync_tree(&self) -> Result<()> {
        #[cfg(not(unix))]
        {
            anyhow::bail!("descriptor-relative tree sync is unavailable on this platform")
        }
        #[cfg(unix)]
        sync_open_directory_tree(&self.path, &self.directory)
    }

    /// Durably sync a tree while bounding every raw observed namespace entry.
    pub fn sync_tree_bounded(&self, budget: DirectoryTraversalBudget) -> Result<()> {
        #[cfg(not(unix))]
        {
            let _ = budget;
            anyhow::bail!("descriptor-relative tree sync is unavailable on this platform")
        }
        #[cfg(unix)]
        {
            let mut remaining = budget.max_entries;
            sync_open_directory_tree_bounded(
                &self.path,
                &self.directory,
                &mut remaining,
                budget.max_depth,
                0,
            )
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FilesystemCapacity {
    pub total_bytes: u64,
    pub available_bytes: u64,
    /// Smallest allocation unit reported for this exact filesystem.
    pub allocation_unit_bytes: u64,
    /// File identities available to an unprivileged writer on this filesystem.
    pub available_files: u64,
}

#[cfg(target_os = "linux")]
fn publish_temp_without_replacement(
    parent: &File,
    temp_name: &std::ffi::CStr,
    target_name: &std::ffi::CStr,
    display_path: &Path,
) -> Result<()> {
    if unsafe {
        libc::renameat2(
            parent.as_raw_fd(),
            temp_name.as_ptr(),
            parent.as_raw_fd(),
            target_name.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error()).with_context(|| {
            format!(
                "publish secure file without replacing {}",
                display_path.display()
            )
        });
    }
    Ok(())
}

#[cfg(all(unix, not(target_os = "linux")))]
fn publish_temp_without_replacement(
    parent: &File,
    temp_name: &std::ffi::CStr,
    target_name: &std::ffi::CStr,
    display_path: &Path,
) -> Result<()> {
    if unsafe {
        libc::linkat(
            parent.as_raw_fd(),
            temp_name.as_ptr(),
            parent.as_raw_fd(),
            target_name.as_ptr(),
            0,
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error()).with_context(|| {
            format!(
                "publish secure file without replacing {}",
                display_path.display()
            )
        });
    }
    if unsafe { libc::unlinkat(parent.as_raw_fd(), temp_name.as_ptr(), 0) } != 0 {
        return Err(std::io::Error::last_os_error()).with_context(|| {
            format!(
                "remove secure publication temp in {}",
                display_path.display()
            )
        });
    }
    Ok(())
}

#[cfg(unix)]
fn validate_child_name(name: &OsStr) -> Result<()> {
    use std::path::Component;
    let mut components = Path::new(name).components();
    if !matches!(components.next(), Some(Component::Normal(_))) || components.next().is_some() {
        anyhow::bail!("secure child name is not one normal path component");
    }
    Ok(())
}

#[cfg(unix)]
fn open_regular_at_flags(
    parent: &File,
    name: &std::ffi::CStr,
    display_path: &Path,
    access_flags: libc::c_int,
    create_flags: libc::c_int,
    mode: u32,
) -> Result<Option<File>> {
    let create = create_flags & libc::O_CREAT != 0;
    let descriptor = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            access_flags | create_flags | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
            mode as libc::mode_t,
        )
    };
    if descriptor < 0 {
        let error = std::io::Error::last_os_error();
        if !create && error.kind() == std::io::ErrorKind::NotFound {
            return Ok(None);
        }
        return Err(error)
            .with_context(|| format!("open secure regular file {}", display_path.display()));
    }
    let file = unsafe { File::from_raw_fd(descriptor) };
    if !file.metadata()?.file_type().is_file() {
        anyhow::bail!(
            "secure input is not a regular file: {}",
            display_path.display()
        );
    }
    Ok(Some(file))
}

#[cfg(unix)]
fn ensure_entry_matches(
    parent: &File,
    name: &std::ffi::CStr,
    expected: Option<&File>,
    display_path: &Path,
) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let current = open_regular_at(parent, name, display_path)?;
    match (expected, current.as_ref()) {
        (None, None) => Ok(()),
        (None, Some(_)) => anyhow::bail!(
            "secure target appeared before publication: {}",
            display_path.display()
        ),
        (Some(_), None) => anyhow::bail!(
            "secure target disappeared before mutation: {}",
            display_path.display()
        ),
        (Some(expected), Some(current)) => {
            let expected = expected.metadata()?;
            let current = current.metadata()?;
            if expected.dev() != current.dev() || expected.ino() != current.ino() {
                anyhow::bail!(
                    "secure target changed before mutation: {}",
                    display_path.display()
                );
            }
            Ok(())
        }
    }
}

#[cfg(unix)]
fn ensure_directory_child_identity(
    parent: &File,
    name: &std::ffi::CStr,
    expected: PinnedDirectoryIdentity,
    display_path: &Path,
) -> Result<()> {
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe {
        libc::fstatat(
            std::os::fd::AsRawFd::as_raw_fd(parent),
            name.as_ptr(),
            &mut stat,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("inspect directory child {}", display_path.display()));
    }
    if stat.st_mode & libc::S_IFMT != libc::S_IFDIR
        || stat.st_dev != expected.containing_device
        || stat.st_ino != expected.inode
    {
        anyhow::bail!(
            "directory child changed before conditional exchange: {}",
            display_path.display()
        );
    }
    Ok(())
}

#[cfg(all(unix, not(target_os = "linux")))]
fn directory_names(_directory: &File) -> Result<Vec<std::ffi::OsString>> {
    anyhow::bail!("secure descriptor-relative directory walking is unavailable on this platform")
}

#[cfg(all(unix, not(target_os = "linux")))]
fn directory_names_with_limit(
    _directory: &File,
    _max_entries: usize,
) -> Result<Vec<std::ffi::OsString>> {
    anyhow::bail!("secure descriptor-relative directory walking is unavailable on this platform")
}

/// Open and read an optional regular file without following any path
/// component.
///
/// `Ok(None)` is returned only when the file or one of its parent directories
/// does not exist. Symlinks, special files, and unsafe path components are
/// errors rather than absence. The returned bytes come from the descriptor
/// opened by the same no-follow observation, so callers do not need a
/// pathname metadata check followed by a separate read.
pub fn read_optional_regular_file_no_follow(path: &Path) -> Result<Option<Vec<u8>>> {
    read_optional_regular_file_bounded_no_follow(path, u64::MAX)
}

/// Open an optional regular file without following links and refuse to read
/// more than `max_bytes`. Missing files or parents return `Ok(None)`;
/// oversized or non-regular inputs are errors.
pub fn read_optional_regular_file_bounded_no_follow(
    path: &Path,
    max_bytes: u64,
) -> Result<Option<Vec<u8>>> {
    #[cfg(not(unix))]
    {
        let _ = (path, max_bytes);
        anyhow::bail!(
            "secure optional bounded no-follow file reading is unavailable on this platform"
        );
    }
    #[cfg(unix)]
    {
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        let Some(directory) = open_directory_no_follow(parent)? else {
            return Ok(None);
        };
        let name = path
            .file_name()
            .ok_or_else(|| anyhow::anyhow!("secure file path has no filename"))?;
        let name = std::ffi::CString::new(name.as_bytes())?;
        let Some(mut file) = open_regular_at(&directory, &name, path)? else {
            return Ok(None);
        };
        let metadata = file.metadata()?;
        if metadata.len() > max_bytes {
            anyhow::bail!("secure file exceeds {max_bytes} bytes: {}", path.display());
        }
        let mut bytes = Vec::with_capacity(usize::try_from(metadata.len()).unwrap_or(0));
        std::io::Read::by_ref(&mut file)
            .take(max_bytes.saturating_add(1))
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > max_bytes {
            anyhow::bail!("secure file exceeds {max_bytes} bytes: {}", path.display());
        }
        Ok(Some(bytes))
    }
}

/// Classify one optional filesystem entry without following any path
/// component or the final entry.
///
/// Missing parents/final entries return `Ok(None)`. Callers can distinguish a
/// real regular file from directories, links, and special files without a
/// pathname metadata/read race.
pub fn inspect_optional_entry_no_follow(path: &Path) -> Result<Option<PinnedEntryType>> {
    #[cfg(not(unix))]
    {
        let _ = path;
        anyhow::bail!("secure optional entry inspection is unavailable on this platform");
    }
    #[cfg(unix)]
    {
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        let Some(directory) = open_directory_no_follow(parent)? else {
            return Ok(None);
        };
        let name = path
            .file_name()
            .ok_or_else(|| anyhow::anyhow!("secure entry path has no filename"))?;
        let name = std::ffi::CString::new(name.as_bytes())?;
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe {
            libc::fstatat(
                directory.as_raw_fd(),
                name.as_ptr(),
                &mut stat,
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } != 0
        {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::NotFound {
                return Ok(None);
            }
            return Err(error).with_context(|| format!("inspect secure entry {}", path.display()));
        }
        let entry_type = match stat.st_mode & libc::S_IFMT {
            libc::S_IFDIR => PinnedEntryType::Directory,
            libc::S_IFREG => PinnedEntryType::Regular,
            libc::S_IFLNK => PinnedEntryType::Symlink,
            libc::S_IFCHR => PinnedEntryType::CharacterDevice,
            libc::S_IFBLK => PinnedEntryType::BlockDevice,
            libc::S_IFIFO => PinnedEntryType::Fifo,
            libc::S_IFSOCK => PinnedEntryType::Socket,
            _ => PinnedEntryType::Other,
        };
        Ok(Some(entry_type))
    }
}

/// Open and read an existing regular file without following any path
/// component. Missing files are errors.
pub fn read_regular_file_no_follow(path: &Path) -> Result<Vec<u8>> {
    read_optional_regular_file_no_follow(path)?
        .ok_or_else(|| anyhow::anyhow!("secure file does not exist: {}", path.display()))
}

/// Open one regular file without following any path component and retain its
/// typed descriptor authority for a later bounded read, CAS capture, mount,
/// or inherited-descriptor conversion.
pub fn open_pinned_regular_file_no_follow(path: &Path) -> Result<PinnedRegularFile> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("regular file has no parent: {}", path.display()))?;
    let name = path
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("regular file has no name: {}", path.display()))?;
    let directory = PinnedDirectory::open(parent)?
        .ok_or_else(|| anyhow::anyhow!("regular-file parent is missing: {}", parent.display()))?;
    directory
        .open_pinned_regular(name, false)?
        .ok_or_else(|| anyhow::anyhow!("secure file does not exist: {}", path.display()))
}

/// Resolve one existing ambient pathname for higher-level namespace policy.
/// The result is diagnostic/semantic path data only; callers that execute or
/// mount content must separately retain a descriptor authority.
pub fn canonicalize_existing_path(path: &Path) -> Result<PathBuf> {
    std::fs::canonicalize(path)
        .with_context(|| format!("canonicalize existing path {}", path.display()))
}

/// Existing host roots that a same-UID sandbox must never receive as a broad
/// writable mount. This is a Linux/host-layout safety floor, not authored
/// workload policy; callers may add narrower protected roots but cannot make
/// these disappear.
pub fn protected_system_write_roots() -> Vec<PathBuf> {
    let mut roots = [
        "/boot", "/dev", "/etc", "/proc", "/run", "/sys", "/usr", "/bin", "/sbin", "/lib", "/lib64",
    ]
    .into_iter()
    .filter_map(|path| canonicalize_existing_path(Path::new(path)).ok())
    .collect::<Vec<_>>();
    roots.sort();
    roots.dedup();
    roots
}

/// Canonical current-user home from the process environment, when usable.
/// This is host diagnostic data only; it never becomes workload authority.
pub fn current_user_home() -> Option<PathBuf> {
    std::env::var_os("HOME").and_then(|home| canonicalize_existing_path(Path::new(&home)).ok())
}

/// Pin one already-canonical absolute mount source with `O_PATH`, without
/// following its final component, and prove the resulting descriptor still
/// resolves to that exact canonical spelling. Raw open/fcntl/procfs mechanics
/// stay entirely inside Lillux.
pub fn pin_canonical_mount_source(
    path: &Path,
) -> Result<crate::exec::InheritedDescriptorAuthority> {
    #[cfg(not(target_os = "linux"))]
    {
        let _ = path;
        anyhow::bail!("descriptor-pinned mount sources require Linux")
    }
    #[cfg(target_os = "linux")]
    {
        use std::ffi::CString;
        use std::os::fd::{AsRawFd as _, FromRawFd as _};
        use std::os::unix::ffi::OsStrExt as _;

        if !path.is_absolute() {
            anyhow::bail!("mount source is not absolute: {}", path.display());
        }
        let encoded = CString::new(path.as_os_str().as_bytes())
            .context("mount source contains an interior NUL")?;
        let lease = crate::exec::retain_fork_sensitive_descriptors();
        let mut descriptor = unsafe {
            libc::open(
                encoded.as_ptr(),
                libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if descriptor < 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("pin mount source {}", path.display()));
        }
        if descriptor <= libc::STDERR_FILENO {
            let duplicate = unsafe { libc::fcntl(descriptor, libc::F_DUPFD_CLOEXEC, 3) };
            let duplicate_error = std::io::Error::last_os_error();
            unsafe {
                libc::close(descriptor);
            }
            if duplicate < 0 {
                return Err(duplicate_error)
                    .with_context(|| format!("move mount descriptor for {}", path.display()));
            }
            descriptor = duplicate;
        }
        // SAFETY: `open`/`fcntl` returned one newly owned descriptor.
        let file = unsafe { File::from_raw_fd(descriptor) };
        open_mount_entry_kind(&file)?;
        let descriptor_path = PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd()));
        let observed = std::fs::read_link(&descriptor_path).with_context(|| {
            format!(
                "resolve pinned mount source {} through {}",
                path.display(),
                descriptor_path.display()
            )
        })?;
        if observed != path {
            anyhow::bail!(
                "mount source {} changed while it was pinned (opened {})",
                path.display(),
                observed.display()
            );
        }
        crate::exec::InheritedDescriptorAuthority::from_owned_file(file, &lease)
            .map_err(anyhow::Error::msg)
    }
}

/// Open an existing regular file without following links and refuse to read
/// more than `max_bytes`. The limit is enforced against both metadata and the
/// bytes actually read from the pinned descriptor.
pub fn read_regular_file_bounded_no_follow(path: &Path, max_bytes: u64) -> Result<Vec<u8>> {
    read_optional_regular_file_bounded_no_follow(path, max_bytes)?
        .ok_or_else(|| anyhow::anyhow!("secure file does not exist: {}", path.display()))
}

/// Read one already-open regular-file descriptor under an exact byte bound.
///
/// Directory walkers use this to keep descriptor-relative observation and
/// allocation limits inside the Lillux OS boundary.
pub fn read_open_regular_file_bounded(mut file: File, max_bytes: u64) -> Result<Vec<u8>> {
    let metadata = file.metadata()?;
    if !metadata.file_type().is_file() {
        anyhow::bail!("open descriptor is not a regular file");
    }
    if metadata.len() > max_bytes {
        anyhow::bail!("open regular file exceeds {max_bytes} bytes");
    }
    let mut bytes = Vec::with_capacity(usize::try_from(metadata.len()).unwrap_or(0));
    std::io::Read::by_ref(&mut file)
        .take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max_bytes {
        anyhow::bail!("open regular file exceeds {max_bytes} bytes");
    }
    Ok(bytes)
}

/// Read an already-open regular file whose descriptor length was observed by
/// the caller before reserving memory.
///
/// The allocation is exactly `expected_bytes`, plus one stack sentinel byte.
/// Positional bounded reads close the concurrent-growth and shared-cursor races
/// without growing beyond the caller's reservation. The descriptor identity
/// and metadata must remain stable, and the sentinel must remain unused.
pub fn read_open_regular_file_exact_bounded(
    mut file: File,
    expected_bytes: u64,
    max_bytes: u64,
) -> Result<Vec<u8>> {
    if expected_bytes > max_bytes {
        anyhow::bail!("expected regular-file length {expected_bytes} exceeds {max_bytes} bytes");
    }
    let before = file.metadata()?;
    if !before.file_type().is_file() {
        anyhow::bail!("open descriptor is not a regular file");
    }
    if before.len() != expected_bytes {
        anyhow::bail!(
            "open regular-file length is {}, expected {expected_bytes}",
            before.len()
        );
    }
    read_open_regular_file_stable_bounded(
        &mut file,
        &OpenRegularFileObservation { metadata: before },
        max_bytes,
    )
}

/// UTF-8 variant of [`read_regular_file_no_follow`].
pub fn read_regular_file_to_string_no_follow(path: &Path) -> Result<String> {
    String::from_utf8(read_regular_file_no_follow(path)?)
        .with_context(|| format!("secure file is not UTF-8: {}", path.display()))
}

/// Deterministically collect every regular file beneath `root` without
/// following symlinks. `recursive=false` rejects child directories. Any
/// symlink or special entry is an error. A missing root yields `None`.
pub fn collect_regular_files_no_follow(
    root: &Path,
    recursive: bool,
) -> Result<Option<Vec<PathBuf>>> {
    #[cfg(not(unix))]
    {
        let _ = (root, recursive);
        anyhow::bail!("secure no-follow directory walking is unavailable on this platform");
    }
    #[cfg(unix)]
    {
        let Some(directory) = open_directory_no_follow(root)? else {
            return Ok(None);
        };
        let mut files = Vec::new();
        collect_from_open_directory(root, &directory, recursive, &mut files)?;
        Ok(Some(files))
    }
}

/// Deterministically collect exact regular-file authorities beneath `root`.
///
/// Unlike [`collect_regular_files_no_follow`], the returned values retain the
/// descriptors opened by the no-follow traversal, so callers cannot
/// accidentally re-open a replaced pathname. The traversal is bounded before
/// descriptors are retained. `recursive=false` rejects child directories;
/// symlinks and special entries always fail closed. A missing root yields
/// `None`.
pub fn collect_pinned_regular_files_no_follow_bounded(
    root: &Path,
    recursive: bool,
    budget: DirectoryTraversalBudget,
) -> Result<Option<Vec<PinnedRegularFile>>> {
    #[cfg(not(unix))]
    {
        let _ = (root, recursive, budget);
        anyhow::bail!(
            "secure descriptor-retaining directory walking is unavailable on this platform"
        );
    }
    #[cfg(unix)]
    {
        let Some(directory) = open_directory_no_follow(root)? else {
            return Ok(None);
        };
        let mut files = Vec::new();
        let mut state = DirectoryTraversalState {
            remaining_entries: budget.max_entries,
            max_depth: budget.max_depth,
        };
        visit_from_open_directory(
            root,
            Path::new(""),
            &directory,
            Some(&mut state),
            0,
            &mut |relative, is_directory| {
                if is_directory && !recursive {
                    anyhow::bail!(
                        "secure flat directory contains unsupported child directory: {}",
                        root.join(relative).display()
                    );
                }
                Ok(false)
            },
            &mut |relative, file| {
                let path = root.join(relative);
                let name = relative
                    .file_name()
                    .ok_or_else(|| anyhow::anyhow!("regular file has no relative filename"))?
                    .to_os_string();
                files.push(PinnedRegularFile { path, name, file });
                Ok(())
            },
        )?;
        Ok(Some(files))
    }
}

/// Complete deterministic directory tree collected through pinned directory
/// descriptors. Paths are relative descendants represented as full paths.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoFollowDirectoryTree {
    pub directories: Vec<PathBuf>,
    pub regular_files: Vec<PathBuf>,
}

/// Collect every descendant directory and regular file beneath `root` without
/// following links. Symlinks and special entries are errors; missing is None.
pub fn collect_directory_tree_no_follow(root: &Path) -> Result<Option<NoFollowDirectoryTree>> {
    #[cfg(not(unix))]
    {
        let _ = root;
        anyhow::bail!("secure no-follow directory walking is unavailable on this platform");
    }
    #[cfg(unix)]
    {
        let Some(directory) = open_directory_no_follow(root)? else {
            return Ok(None);
        };
        let mut tree = NoFollowDirectoryTree {
            directories: Vec::new(),
            regular_files: Vec::new(),
        };
        collect_tree_from_open_directory(root, &directory, &mut tree)?;
        Ok(Some(tree))
    }
}

/// Visit every included regular file below an exact pinned root. `prune`
/// receives a canonical relative path and whether the entry is a directory;
/// returning true skips that entry (and a directory's complete subtree).
/// Symlinks and special files fail closed.
pub fn visit_regular_files_no_follow<P, V>(root: &Path, mut prune: P, mut visit: V) -> Result<bool>
where
    P: FnMut(&Path, bool) -> Result<bool>,
    V: FnMut(&Path, File) -> Result<()>,
{
    #[cfg(not(unix))]
    {
        let _ = (root, &mut prune, &mut visit);
        anyhow::bail!("secure no-follow directory walking is unavailable on this platform")
    }
    #[cfg(unix)]
    {
        let Some(directory) = open_directory_no_follow(root)? else {
            return Ok(false);
        };
        visit_from_open_directory(
            root,
            Path::new(""),
            &directory,
            None,
            0,
            &mut prune,
            &mut visit,
        )?;
        Ok(true)
    }
}

/// Bounded form of [`visit_regular_files_no_follow`]. Missing roots return
/// `false`; present roots fail before exceeding the supplied entry/depth
/// budget.
pub fn visit_regular_files_no_follow_bounded<P, V>(
    root: &Path,
    budget: DirectoryTraversalBudget,
    mut prune: P,
    mut visit: V,
) -> Result<bool>
where
    P: FnMut(&Path, bool) -> Result<bool>,
    V: FnMut(&Path, File) -> Result<()>,
{
    #[cfg(not(unix))]
    {
        let _ = (root, budget, &mut prune, &mut visit);
        anyhow::bail!("secure no-follow directory walking is unavailable on this platform")
    }
    #[cfg(unix)]
    {
        let Some(directory) = open_directory_no_follow(root)? else {
            return Ok(false);
        };
        let mut state = DirectoryTraversalState {
            remaining_entries: budget.max_entries,
            max_depth: budget.max_depth,
        };
        visit_from_open_directory(
            root,
            Path::new(""),
            &directory,
            Some(&mut state),
            0,
            &mut prune,
            &mut visit,
        )?;
        Ok(true)
    }
}

#[cfg(unix)]
fn visit_from_open_directory<P, V>(
    root: &Path,
    relative_directory: &Path,
    directory: &File,
    mut budget: Option<&mut DirectoryTraversalState>,
    depth: usize,
    prune: &mut P,
    visit: &mut V,
) -> Result<()>
where
    P: FnMut(&Path, bool) -> Result<bool>,
    V: FnMut(&Path, File) -> Result<()>,
{
    let names = if let Some(state) = budget.as_deref_mut() {
        let names = directory_names_with_limit(directory, state.remaining_entries)?;
        state.remaining_entries = state.remaining_entries.saturating_sub(names.len());
        names
    } else {
        directory_names(directory)?
    };
    for name in names {
        let relative = relative_directory.join(&name);
        let display = root.join(&relative);
        let name_c = std::ffi::CString::new(name.as_bytes())?;
        if let Some(child_directory) = open_child_directory(directory, &name_c, &display)? {
            if !prune(&relative, true)? {
                if let Some(state) = budget.as_ref()
                    && depth >= state.max_depth
                {
                    anyhow::bail!(
                        "secure directory traversal exceeds maximum depth {} at {}",
                        state.max_depth,
                        display.display()
                    );
                }
                visit_from_open_directory(
                    root,
                    &relative,
                    &child_directory,
                    budget.as_deref_mut(),
                    depth.saturating_add(1),
                    prune,
                    visit,
                )?;
            }
            continue;
        }
        let file = open_regular_at(directory, &name_c, &display)?.ok_or_else(|| {
            anyhow::anyhow!(
                "secure project walk encountered a symlink, special file, or disappearing entry: {}",
                display.display()
            )
        })?;
        if prune(&relative, false)? {
            continue;
        }
        visit(&relative, file)?;
    }
    Ok(())
}

#[cfg(unix)]
fn copy_open_directory_filtered<P>(
    source: &PinnedDirectory,
    destination: &PinnedDirectory,
    relative_directory: &Path,
    depth: usize,
    state: &mut DirectoryTraversalState,
    exclude: &mut P,
) -> Result<()>
where
    P: FnMut(&Path) -> Result<bool>,
{
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

    if !destination.entries_no_follow_bounded(0)?.is_empty() {
        anyhow::bail!(
            "secure tree-copy destination is not empty: {}",
            destination.path.display()
        );
    }
    let source_before = source.directory.metadata()?;
    let initial = source.entries_no_follow_bounded(state.remaining_entries)?;
    state.remaining_entries = state.remaining_entries.saturating_sub(initial.len());
    for entry in &initial {
        let relative = relative_directory.join(&entry.name);
        if exclude(&relative)? {
            continue;
        }
        match entry.entry_type {
            PinnedEntryType::Directory => {
                if depth >= state.max_depth {
                    anyhow::bail!(
                        "secure tree copy exceeds maximum depth {} at {}",
                        state.max_depth,
                        source.path.join(&entry.name).display()
                    );
                }
                let source_child = source
                    .open_child_directory(&entry.name)?
                    .ok_or_else(|| anyhow::anyhow!("source directory disappeared during copy"))?;
                let source_child_identity = source_child.identity()?;
                if source_child_identity.containing_device != entry.containing_device
                    || source_child_identity.inode != entry.inode
                {
                    anyhow::bail!("source directory changed identity during copy");
                }
                let metadata = source_child.directory.metadata()?;
                let mode = metadata.mode() & 0o7777;
                let destination_child = destination.create_child(&entry.name, mode)?;
                destination_child
                    .directory
                    .set_permissions(std::fs::Permissions::from_mode(mode))?;
                copy_open_directory_filtered(
                    &source_child,
                    &destination_child,
                    &relative,
                    depth + 1,
                    state,
                    exclude,
                )?;
                let times = std::fs::FileTimes::new()
                    .set_accessed(metadata.accessed()?)
                    .set_modified(metadata.modified()?);
                destination_child.directory.set_times(times)?;
                destination_child.directory.sync_all()?;
                source.ensure_entry_observation(entry)?;
            }
            PinnedEntryType::Regular => {
                let source_file = source
                    .open_regular(&entry.name, false)?
                    .ok_or_else(|| anyhow::anyhow!("source file disappeared during copy"))?;
                let observation = observe_open_regular_file(&source_file)?;
                if !observation.matches_directory_entry(entry) {
                    anyhow::bail!("source file changed identity during copy");
                }
                copy_open_regular_into_new(destination, &entry.name, source_file, &observation)?;
                source.ensure_entry_observation(entry)?;
            }
            PinnedEntryType::Symlink => anyhow::bail!(
                "secure tree copy refuses symlink {}",
                source.path.join(&entry.name).display()
            ),
            other => anyhow::bail!(
                "secure tree copy refuses {other:?} entry {}",
                source.path.join(&entry.name).display()
            ),
        }
    }
    if source.entries_no_follow_bounded(initial.len())? != initial {
        anyhow::bail!("source directory changed during secure tree copy");
    }
    let source_after = source.directory.metadata()?;
    if !same_regular_file_observation(&source_before, &source_after) {
        anyhow::bail!("source directory metadata changed during secure tree copy");
    }
    let mode = source_before.mode() & 0o7777;
    destination
        .directory
        .set_permissions(std::fs::Permissions::from_mode(mode))?;
    let times = std::fs::FileTimes::new()
        .set_accessed(source_before.accessed()?)
        .set_modified(source_before.modified()?);
    destination.directory.set_times(times)?;
    destination.directory.sync_all()?;
    Ok(())
}

#[cfg(unix)]
fn copy_open_regular_into_new(
    destination: &PinnedDirectory,
    name: &OsStr,
    mut source: File,
    observation: &OpenRegularFileObservation,
) -> Result<()> {
    use std::os::fd::{AsRawFd as _, FromRawFd as _};
    use std::os::unix::ffi::OsStrExt as _;
    use std::os::unix::fs::PermissionsExt as _;

    validate_child_name(name)?;
    let name_c = std::ffi::CString::new(name.as_bytes())?;
    let descriptor = unsafe {
        libc::openat(
            destination.directory.as_raw_fd(),
            name_c.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0,
        )
    };
    if descriptor < 0 {
        return Err(std::io::Error::last_os_error()).with_context(|| {
            format!(
                "create secure copied file {}",
                destination.path.join(name).display()
            )
        });
    }
    let mut target = unsafe { File::from_raw_fd(descriptor) };
    let result = (|| -> Result<()> {
        source.seek(std::io::SeekFrom::Start(0))?;
        let mut bounded = (&mut source).take(observation.size().saturating_add(1));
        let copied = std::io::copy(&mut bounded, &mut target)?;
        if copied != observation.size() {
            anyhow::bail!("source file changed size during secure tree copy");
        }
        ensure_open_regular_file_unchanged(&source, observation)?;
        let mode = observation.permission_mode()?;
        target.set_permissions(std::fs::Permissions::from_mode(mode))?;
        let metadata = &observation.metadata;
        let times = std::fs::FileTimes::new()
            .set_accessed(metadata.accessed()?)
            .set_modified(metadata.modified()?);
        target.set_times(times)?;
        target.sync_all()?;
        destination.directory.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        unsafe {
            libc::unlinkat(destination.directory.as_raw_fd(), name_c.as_ptr(), 0);
        }
    }
    result
}

#[cfg(unix)]
fn sync_open_directory_tree(path: &Path, directory: &File) -> Result<()> {
    for name in directory_names(directory)? {
        let child_path = path.join(&name);
        let name = std::ffi::CString::new(name.as_bytes())?;
        if let Some(child_directory) = open_child_directory(directory, &name, &child_path)? {
            sync_open_directory_tree(&child_path, &child_directory)?;
            continue;
        }
        let file = open_regular_at(directory, &name, &child_path)
            .with_context(|| {
                format!(
                    "secure tree sync rejected a symlink or non-regular entry: {}",
                    child_path.display()
                )
            })?
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "secure tree sync encountered a disappearing entry: {}",
                    child_path.display()
                )
            })?;
        file.sync_all()
            .with_context(|| format!("sync secure regular file {}", child_path.display()))?;
    }
    directory
        .sync_all()
        .with_context(|| format!("sync secure directory {}", path.display()))
}

#[cfg(unix)]
fn sync_open_directory_tree_bounded(
    path: &Path,
    directory: &File,
    remaining: &mut usize,
    max_depth: usize,
    depth: usize,
) -> Result<()> {
    if depth > max_depth {
        anyhow::bail!("secure tree sync exceeds its directory depth bound");
    }
    let names = directory_names_with_limit(directory, *remaining)?;
    *remaining = remaining
        .checked_sub(names.len())
        .ok_or_else(|| anyhow::anyhow!("secure tree sync entry budget underflow"))?;
    for name in names {
        let child_path = path.join(&name);
        let name = std::ffi::CString::new(name.as_bytes())?;
        if let Some(child_directory) = open_child_directory(directory, &name, &child_path)? {
            sync_open_directory_tree_bounded(
                &child_path,
                &child_directory,
                remaining,
                max_depth,
                depth + 1,
            )?;
            continue;
        }
        let file = open_regular_at(directory, &name, &child_path)
            .with_context(|| {
                format!(
                    "secure tree sync rejected a symlink or non-regular entry: {}",
                    child_path.display()
                )
            })?
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "secure tree sync encountered a disappearing entry: {}",
                    child_path.display()
                )
            })?;
        file.sync_all()
            .with_context(|| format!("sync secure regular file {}", child_path.display()))?;
    }
    directory
        .sync_all()
        .with_context(|| format!("sync secure directory {}", path.display()))
}

#[cfg(unix)]
fn collect_from_open_directory(
    path: &Path,
    directory: &File,
    recursive: bool,
    files: &mut Vec<PathBuf>,
) -> Result<()> {
    for name in directory_names(directory)? {
        let child_path = path.join(&name);
        let name = std::ffi::CString::new(name.as_bytes())?;
        if let Some(child_directory) = open_child_directory(directory, &name, &child_path)? {
            if !recursive {
                anyhow::bail!(
                    "secure flat directory contains unsupported child directory: {}",
                    child_path.display()
                );
            }
            collect_from_open_directory(&child_path, &child_directory, true, files)?;
            continue;
        }
        open_regular_at(directory, &name, &child_path)?
            .ok_or_else(|| anyhow::anyhow!("secure directory entry disappeared"))?;
        files.push(child_path);
    }
    Ok(())
}

#[cfg(unix)]
fn collect_tree_from_open_directory(
    path: &Path,
    directory: &File,
    tree: &mut NoFollowDirectoryTree,
) -> Result<()> {
    for name in directory_names(directory)? {
        let child_path = path.join(&name);
        let name = std::ffi::CString::new(name.as_bytes())?;
        if let Some(child_directory) = open_child_directory(directory, &name, &child_path)? {
            tree.directories.push(child_path.clone());
            collect_tree_from_open_directory(&child_path, &child_directory, tree)?;
            continue;
        }
        open_regular_at(directory, &name, &child_path)?
            .ok_or_else(|| anyhow::anyhow!("secure directory entry disappeared"))?;
        tree.regular_files.push(child_path);
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn rename_noreplace_between(
    source_directory: &File,
    source_name: &std::ffi::CStr,
    target_directory: &File,
    target_name: &std::ffi::CStr,
) -> std::io::Result<()> {
    if unsafe {
        libc::renameat2(
            source_directory.as_raw_fd(),
            source_name.as_ptr(),
            target_directory.as_raw_fd(),
            target_name.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn restore_quarantined_regular(
    directory: &File,
    quarantine_name: &std::ffi::CStr,
    target_name: &std::ffi::CStr,
) -> Result<()> {
    rename_noreplace_between(directory, quarantine_name, directory, target_name)
        .context("restore quarantined regular file")?;
    directory.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn owned_host_files_reject_writable_modes_wrong_owners_and_links() {
        use std::os::unix::fs::PermissionsExt as _;
        let temporary = tempfile::tempdir().unwrap();
        let root = PinnedDirectory::open(temporary.path()).unwrap().unwrap();
        let owner = unsafe { libc::geteuid() };
        root.require_owner(owner).unwrap();
        assert!(root.require_owner(owner.wrapping_add(1)).is_err());
        std::fs::set_permissions(temporary.path(), std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(root.require_owner(owner).is_err());
        std::fs::set_permissions(temporary.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = temporary.path().join("host.json");
        std::fs::write(&path, b"exact").unwrap();
        let file = root
            .open_pinned_regular(OsStr::new("host.json"), false)
            .unwrap()
            .unwrap();
        file.require_owner(owner).unwrap();
        assert!(file.require_owner(owner.wrapping_add(1)).is_err());
        file.set_mode(0o666).unwrap();
        assert!(file.require_owner(owner).is_err());
        file.set_mode(0o600).unwrap();
        std::fs::hard_link(&path, temporary.path().join("alias")).unwrap();
        assert!(file.require_owner(owner).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn host_directory_discovery_distinguishes_absence_from_occupied_names() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let directory = PinnedDirectory::open(root.path()).unwrap().unwrap();
        assert!(
            directory
                .open_child_directory(OsStr::new("absent"))
                .unwrap()
                .is_none()
        );
        std::fs::write(root.path().join("file"), b"occupied").unwrap();
        std::fs::create_dir(root.path().join("real-directory")).unwrap();
        symlink("real-directory", root.path().join("directory-link")).unwrap();
        symlink("missing", root.path().join("dangling-link")).unwrap();
        for name in ["file", "directory-link", "dangling-link"] {
            assert!(
                directory.open_child_directory(OsStr::new(name)).is_err(),
                "{name} must not become direct-node fallback"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn pinned_controller_cwd_survives_path_replacement() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("node");
        std::fs::create_dir(&path).unwrap();
        std::fs::write(path.join("marker"), b"retained").unwrap();
        let root = PinnedDirectory::open(&path).unwrap().unwrap();
        let mut command = std::process::Command::new("/bin/sh");
        command.env_clear().args(["-c", "test -f marker"]);
        root.configure_command_cwd(&mut command).unwrap();
        std::fs::rename(&path, temporary.path().join("old-node")).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(root.ensure_path_binding().is_err());
        assert!(command.status().unwrap().success());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn read_only_namespace_path_rejects_noncanonical_spelling_before_mount_checks() {
        let held = tempfile::tempfile().unwrap();
        for path in [
            "relative",
            "/tmp/../member",
            "/tmp/./member",
            "/tmp//member",
            "/tmp/",
        ] {
            let error = verified_read_only_namespace_path(Path::new(path), &held).unwrap_err();
            assert!(
                error.to_string().contains("absolute and normalized"),
                "{path}: {error}"
            );
        }
        let oversized = format!("/{}", "x".repeat(libc::PATH_MAX as usize));
        assert!(verified_read_only_namespace_path(Path::new(&oversized), &held).is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn read_only_namespace_object_check_requires_exact_held_inode_and_type() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("member");
        std::fs::write(&path, b"same bytes").unwrap();
        let held = File::open(&path).unwrap();
        require_same_namespace_object(&held, &File::open(&path).unwrap()).unwrap();
        std::fs::rename(&path, root.path().join("prior")).unwrap();
        std::fs::write(&path, b"same bytes").unwrap();
        assert!(require_same_namespace_object(&held, &File::open(&path).unwrap()).is_err());
        assert!(require_same_namespace_object(&held, &File::open(root.path()).unwrap()).is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn writable_mount_namespace_path_requires_directory_leaf_and_immutable_parent() {
        let root = tempfile::tempdir().unwrap();
        let pinned = PinnedDirectory::open(root.path()).unwrap().unwrap();
        assert!(
            pinned
                .verified_writable_mount_namespace_path(root.path())
                .is_err()
        );
        assert!(
            pinned
                .verified_writable_mount_namespace_path(Path::new("relative"))
                .is_err()
        );
        let error = pinned
            .verified_writable_mount_namespace_path(Path::new("/"))
            .unwrap_err();
        assert!(error.to_string().contains("non-root pinned directory"));
        let file = tempfile::tempfile().unwrap();
        let error = verified_namespace_path(
            Path::new("/leaf"),
            &file,
            NamespaceLeafAccess::WritableDirectory,
        )
        .unwrap_err();
        assert!(error.to_string().contains("non-root pinned directory"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn read_only_namespace_path_rejects_writable_mount_and_symlink_rebinding() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("member");
        std::fs::write(&path, b"exact").unwrap();
        let held = open_pinned_regular_file_no_follow(&path).unwrap();
        assert!(held.verified_read_only_namespace_path().is_err());
        let directory = PinnedDirectory::open(root.path()).unwrap().unwrap();
        assert!(directory.verified_read_only_namespace_path().is_err());
        std::fs::rename(&path, root.path().join("prior")).unwrap();
        std::os::unix::fs::symlink("prior", &path).unwrap();
        assert!(held.verified_read_only_namespace_path().is_err());
        assert!(open_pinned_regular_file_no_follow(&path).is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn process_scoped_flat_generations_retain_live_peers_and_remove_exact_drop() {
        let root = tempfile::tempdir().unwrap();
        let pinned = PinnedDirectory::open(root.path()).unwrap().unwrap();
        let first = ProcessScopedFlatDirectoryGeneration::create_relative(
            &pinned,
            &["state", "cache", "generations"],
        )
        .unwrap();
        let first_path = first.directory().path().to_path_buf();
        let second = ProcessScopedFlatDirectoryGeneration::create_relative(
            &pinned,
            &["state", "cache", "generations"],
        )
        .unwrap();
        let second_path = second.directory().path().to_path_buf();
        assert!(first_path.is_dir());
        assert!(second_path.is_dir());

        drop(first);
        assert!(!first_path.exists());
        assert!(second_path.is_dir());
        drop(second);
        assert!(!second_path.exists());
    }

    #[test]
    fn stable_exact_digest_rejects_size_drift_before_body_work() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("value");
        std::fs::write(&path, b"exact").unwrap();
        let mut file = File::open(&path).unwrap();
        let (digest, metadata) =
            digest_open_regular_file_stable_exact(&mut file, b"exact".len() as u64).unwrap();
        assert_eq!(digest, crate::sha256_hex(b"exact"));
        assert_eq!(metadata.len(), b"exact".len() as u64);

        let mut file = File::open(&path).unwrap();
        assert!(digest_open_regular_file_stable_exact(&mut file, 1).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn optional_no_follow_file_contract_covers_absence_links_and_special_entries() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let regular = root.path().join("regular");
        std::fs::write(&regular, b"value").unwrap();
        assert_eq!(
            read_optional_regular_file_no_follow(&regular).unwrap(),
            Some(b"value".to_vec())
        );
        assert_eq!(
            read_optional_regular_file_no_follow(&root.path().join("missing")).unwrap(),
            None
        );
        assert_eq!(
            read_optional_regular_file_no_follow(&root.path().join("missing-parent/value"))
                .unwrap(),
            None
        );

        let final_link = root.path().join("final-link");
        symlink(&regular, &final_link).unwrap();
        assert!(read_optional_regular_file_no_follow(&final_link).is_err());
        assert_eq!(
            inspect_optional_entry_no_follow(&final_link).unwrap(),
            Some(PinnedEntryType::Symlink)
        );

        let real_parent = root.path().join("real-parent");
        std::fs::create_dir(&real_parent).unwrap();
        std::fs::write(real_parent.join("value"), b"nested").unwrap();
        let parent_link = root.path().join("parent-link");
        symlink(&real_parent, &parent_link).unwrap();
        assert!(read_optional_regular_file_no_follow(&parent_link.join("value")).is_err());
        assert!(inspect_optional_entry_no_follow(&parent_link.join("value")).is_err());

        let directory = root.path().join("directory");
        std::fs::create_dir(&directory).unwrap();
        assert_eq!(
            inspect_optional_entry_no_follow(&directory).unwrap(),
            Some(PinnedEntryType::Directory)
        );
        assert!(read_optional_regular_file_no_follow(&directory).is_err());

        let fifo = root.path().join("fifo");
        let fifo_c = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo_c.as_ptr(), 0o600) }, 0);
        assert_eq!(
            inspect_optional_entry_no_follow(&fifo).unwrap(),
            Some(PinnedEntryType::Fifo)
        );
        assert!(read_optional_regular_file_no_follow(&fifo).is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn conditional_regular_replace_preserves_a_racing_target() {
        let root = tempfile::tempdir().unwrap();
        let destination_path = root.path().join("destination");
        let source_path = root.path().join("source");
        std::fs::create_dir(&destination_path).unwrap();
        std::fs::create_dir(&source_path).unwrap();
        std::fs::write(destination_path.join("value"), b"base").unwrap();
        std::fs::write(source_path.join("staged"), b"remote").unwrap();
        let destination = PinnedDirectory::open(&destination_path).unwrap().unwrap();
        let source = PinnedDirectory::open(&source_path).unwrap().unwrap();
        let expected = destination
            .open_regular(OsStr::new("value"), false)
            .unwrap()
            .unwrap();
        let staged = source
            .open_regular(OsStr::new("staged"), false)
            .unwrap()
            .unwrap();
        std::fs::remove_file(destination_path.join("value")).unwrap();
        std::fs::write(destination_path.join("value"), b"local edit").unwrap();

        assert!(
            destination
                .replace_regular_from_if_matches_atomic(
                    OsStr::new("value"),
                    Some(&expected),
                    |_| Ok(()),
                    &source,
                    OsStr::new("staged"),
                    &staged,
                )
                .is_err()
        );
        assert_eq!(
            std::fs::read(destination_path.join("value")).unwrap(),
            b"local edit"
        );
        assert_eq!(
            std::fs::read(source_path.join("staged")).unwrap(),
            b"remote"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn conditional_byte_replace_validates_at_the_namespace_boundary_and_preserves_mode() {
        use std::os::unix::fs::PermissionsExt as _;

        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("value");
        std::fs::write(&target, b"base").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o664)).unwrap();
        let directory = PinnedDirectory::open(root.path()).unwrap().unwrap();
        let expected = directory
            .open_regular(OsStr::new("value"), false)
            .unwrap()
            .unwrap();
        let observed = observe_open_regular_file(&expected).unwrap();

        directory
            .replace_bytes_if_matches_atomic(
                OsStr::new("value"),
                Some(&expected),
                |current| {
                    let current = observe_open_regular_file(current)?;
                    anyhow::ensure!(current.matches_quarantined_incumbent(&observed));
                    Ok(())
                },
                b"authored",
                observed.permission_mode().unwrap(),
            )
            .unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"authored");
        assert_eq!(
            std::fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o664
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn conditional_byte_replace_does_not_overwrite_a_rename_editor() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("value");
        std::fs::write(&target, b"base").unwrap();
        let directory = PinnedDirectory::open(root.path()).unwrap().unwrap();
        let expected = directory
            .open_regular(OsStr::new("value"), false)
            .unwrap()
            .unwrap();
        std::fs::rename(&target, root.path().join("old")).unwrap();
        std::fs::write(&target, b"concurrent").unwrap();

        assert!(
            directory
                .replace_bytes_if_matches_atomic(
                    OsStr::new("value"),
                    Some(&expected),
                    |_| Ok(()),
                    b"authored",
                    0o644,
                )
                .is_err()
        );
        assert_eq!(std::fs::read(&target).unwrap(), b"concurrent");
    }

    #[cfg(target_os = "linux")]
    fn install_authoring_recovery_fixture(
        directory: &PinnedDirectory,
        target_name: &str,
        stage_name: &str,
        quarantine_name: &str,
        expected: RegularFileIdentity,
        staged: RegularFileIdentity,
    ) {
        let marker_name = PinnedDirectory::conditional_byte_recovery_name(OsStr::new(target_name));
        let bytes = serde_json::to_vec(&ConditionalByteReplacementRecovery {
            schema: 1,
            target_name: target_name.as_bytes().to_vec(),
            stage_name: stage_name.as_bytes().to_vec(),
            quarantine_name: quarantine_name.as_bytes().to_vec(),
            expected_target: expected,
            staged_target: staged,
        })
        .unwrap();
        directory
            .atomic_write_if_same(&marker_name, None, &bytes, 0o600)
            .unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn conditional_byte_recovery_restores_an_interrupted_precommit() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("value"), b"old").unwrap();
        std::fs::write(root.path().join("stage"), b"new").unwrap();
        let directory = PinnedDirectory::open(root.path()).unwrap().unwrap();
        let old = directory
            .open_regular(OsStr::new("value"), false)
            .unwrap()
            .unwrap();
        let stage = directory
            .open_regular(OsStr::new("stage"), false)
            .unwrap()
            .unwrap();
        install_authoring_recovery_fixture(
            &directory,
            "value",
            "stage",
            "quarantine",
            regular_file_identity(&old).unwrap(),
            regular_file_identity(&stage).unwrap(),
        );
        std::fs::rename(root.path().join("value"), root.path().join("quarantine")).unwrap();

        directory
            .recover_conditional_byte_replacement_atomic(OsStr::new("value"))
            .unwrap();
        assert_eq!(std::fs::read(root.path().join("value")).unwrap(), b"old");
        assert!(!root.path().join("stage").exists());
        assert!(!root.path().join("quarantine").exists());
        assert!(
            !root
                .path()
                .join(PinnedDirectory::conditional_byte_recovery_name(OsStr::new(
                    "value"
                )))
                .exists()
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn conditional_byte_recovery_finishes_a_committed_replacement() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("value"), b"old").unwrap();
        std::fs::write(root.path().join("stage"), b"new").unwrap();
        let directory = PinnedDirectory::open(root.path()).unwrap().unwrap();
        let old = directory
            .open_regular(OsStr::new("value"), false)
            .unwrap()
            .unwrap();
        let stage = directory
            .open_regular(OsStr::new("stage"), false)
            .unwrap()
            .unwrap();
        install_authoring_recovery_fixture(
            &directory,
            "value",
            "stage",
            "quarantine",
            regular_file_identity(&old).unwrap(),
            regular_file_identity(&stage).unwrap(),
        );
        std::fs::rename(root.path().join("value"), root.path().join("quarantine")).unwrap();
        std::fs::rename(root.path().join("stage"), root.path().join("value")).unwrap();

        directory
            .recover_conditional_byte_replacement_atomic(OsStr::new("value"))
            .unwrap();
        assert_eq!(std::fs::read(root.path().join("value")).unwrap(), b"new");
        assert!(!root.path().join("quarantine").exists());
        assert!(
            !root
                .path()
                .join(PinnedDirectory::conditional_byte_recovery_name(OsStr::new(
                    "value"
                )))
                .exists()
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn prior_ambiguous_recovery_is_never_reported_as_the_current_commit() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("value"), b"old").unwrap();
        std::fs::write(root.path().join("stage"), b"previous-new").unwrap();
        let directory = PinnedDirectory::open(root.path()).unwrap().unwrap();
        let old = directory
            .open_regular(OsStr::new("value"), false)
            .unwrap()
            .unwrap();
        let stage = directory
            .open_regular(OsStr::new("stage"), false)
            .unwrap()
            .unwrap();
        install_authoring_recovery_fixture(
            &directory,
            "value",
            "stage",
            "quarantine",
            regular_file_identity(&old).unwrap(),
            regular_file_identity(&stage).unwrap(),
        );
        std::fs::rename(root.path().join("value"), root.path().join("unrelated-old")).unwrap();
        std::fs::write(root.path().join("value"), b"racing-value").unwrap();
        let current = directory
            .open_regular(OsStr::new("value"), false)
            .unwrap()
            .unwrap();

        let error = directory
            .replace_bytes_if_matches_atomic(
                OsStr::new("value"),
                Some(&current),
                |_| Ok(()),
                b"current-request",
                0o600,
            )
            .unwrap_err();
        assert!(!error.namespace_committed());
        assert!(error.to_string().contains("prior conditional authoring"));
        assert_eq!(
            std::fs::read(root.path().join("value")).unwrap(),
            b"racing-value"
        );
    }

    #[test]
    fn stable_bounded_read_rejects_a_changed_observation() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("value");
        std::fs::write(&path, b"first").unwrap();
        let mut file = File::open(&path).unwrap();
        let observed = observe_open_regular_file(&file).unwrap();
        std::fs::write(&path, b"second-value").unwrap();
        assert!(read_open_regular_file_stable_bounded(&mut file, &observed, 1024).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn exact_descriptor_reads_leave_a_shared_cursor_unchanged() {
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(b"exact retained bytes").unwrap();
        file.seek(std::io::SeekFrom::Start(4)).unwrap();
        let observed = observe_open_regular_file(&file).unwrap();
        let mut clone = file.try_clone().unwrap();
        assert_eq!(
            read_open_regular_file_stable_bounded(&mut clone, &observed, 128).unwrap(),
            b"exact retained bytes"
        );
        assert_eq!(file.stream_position().unwrap(), 4);
        let (digest, _) =
            digest_open_regular_file_stable_exact(&mut clone, observed.size()).unwrap();
        assert_eq!(digest, crate::cas::sha256_hex(b"exact retained bytes"));
        assert_eq!(file.stream_position().unwrap(), 4);
        assert_eq!(
            read_open_regular_file_exact_bounded(clone, observed.size(), 128).unwrap(),
            b"exact retained bytes"
        );
        assert_eq!(file.stream_position().unwrap(), 4);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn conditional_regular_remove_preserves_a_racing_target() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("value"), b"base").unwrap();
        let directory = PinnedDirectory::open(root.path()).unwrap().unwrap();
        let expected = directory
            .open_regular(OsStr::new("value"), false)
            .unwrap()
            .unwrap();
        std::fs::remove_file(root.path().join("value")).unwrap();
        std::fs::write(root.path().join("value"), b"local edit").unwrap();

        assert!(
            directory
                .remove_if_same_atomic(OsStr::new("value"), &expected)
                .is_err()
        );
        assert_eq!(
            std::fs::read(root.path().join("value")).unwrap(),
            b"local edit"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn typed_pinned_regular_remove_refuses_a_rebound_name() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("value");
        std::fs::write(&path, b"observed").unwrap();
        let directory = PinnedDirectory::open(root.path()).unwrap().unwrap();
        let expected = directory
            .open_pinned_regular(OsStr::new("value"), false)
            .unwrap()
            .unwrap();

        std::fs::rename(&path, root.path().join("displaced")).unwrap();
        std::fs::write(&path, b"replacement").unwrap();

        assert!(directory.remove_pinned_regular_if_same(&expected).is_err());
        assert_eq!(std::fs::read(path).unwrap(), b"replacement");
    }

    #[cfg(unix)]
    #[test]
    fn reader_and_walker_reject_symlinked_ancestors() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let outside = dir.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("value.yaml"), b"value").unwrap();
        let linked = dir.path().join("linked");
        symlink(&outside, &linked).unwrap();

        assert!(read_regular_file_no_follow(&linked.join("value.yaml")).is_err());
        assert!(collect_regular_files_no_follow(&linked, true).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn symlink_targets_are_readable_without_following_and_bounded() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("value"), b"value").unwrap();
        symlink("value", dir.path().join("relative")).unwrap();
        symlink("/usr/bin/python3", dir.path().join("escaping")).unwrap();
        let pinned = PinnedDirectory::open(dir.path()).unwrap().unwrap();

        // Targets are returned verbatim, including one that leaves the space:
        // recording a link is not following it.
        assert_eq!(
            pinned
                .read_symlink_target(OsStr::new("relative"), 1024)
                .unwrap()
                .unwrap(),
            b"value".to_vec()
        );
        assert_eq!(
            pinned
                .read_symlink_target(OsStr::new("escaping"), 1024)
                .unwrap()
                .unwrap(),
            b"/usr/bin/python3".to_vec()
        );

        // A regular file is not a link, and an absent name is not an error.
        assert!(
            pinned
                .read_symlink_target(OsStr::new("value"), 1024)
                .unwrap()
                .is_none()
        );
        assert!(
            pinned
                .read_symlink_target(OsStr::new("missing"), 1024)
                .unwrap()
                .is_none()
        );

        // An oversized target fails rather than being silently truncated into
        // a different link.
        assert!(
            pinned
                .read_symlink_target(OsStr::new("escaping"), 4)
                .is_err()
        );
    }

    #[test]
    fn pinned_entry_metadata_reports_containing_device_and_inode() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("value"), b"value").unwrap();
        let pinned = PinnedDirectory::open(dir.path()).unwrap().unwrap();
        let (root_device, _) = pinned.device_inode().unwrap();

        let entries = pinned.entries_no_follow().unwrap();
        let entry = entries
            .iter()
            .find(|entry| entry.name == OsStr::new("value"))
            .unwrap();

        // A traversal that must stay on one filesystem compares this against
        // its pinned root; `device_id` (st_rdev) cannot answer that question.
        assert_eq!(entry.containing_device, root_device);
        assert_ne!(entry.inode, 0);
        assert_eq!(entry.device_id, 0);
    }

    #[test]
    fn pinned_mixed_entry_open_distinguishes_files_and_directories_and_rejects_links() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("child")).unwrap();
        std::fs::write(dir.path().join("value"), b"value").unwrap();
        symlink(dir.path().join("value"), dir.path().join("linked")).unwrap();
        let pinned = PinnedDirectory::open(dir.path()).unwrap().unwrap();

        assert!(matches!(
            pinned.open_entry(OsStr::new("child"), false).unwrap(),
            Some(PinnedDirectoryEntry::Directory(_))
        ));
        assert!(matches!(
            pinned.open_entry(OsStr::new("value"), false).unwrap(),
            Some(PinnedDirectoryEntry::Regular(_))
        ));
        assert!(pinned.open_entry(OsStr::new("linked"), false).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn conditional_mutations_reject_a_swapped_target_inode() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("schedule.yaml");
        std::fs::write(&target, b"verified").unwrap();
        let directory = PinnedDirectory::open(dir.path()).unwrap().unwrap();
        let expected = directory
            .open_regular(OsStr::new("schedule.yaml"), false)
            .unwrap()
            .unwrap();

        std::fs::rename(&target, dir.path().join("old.yaml")).unwrap();
        std::fs::write(&target, b"replacement").unwrap();

        assert!(
            directory
                .atomic_write_if_same(
                    OsStr::new("schedule.yaml"),
                    Some(&expected),
                    b"desired",
                    0o600,
                )
                .is_err()
        );
        assert!(
            directory
                .remove_if_same(OsStr::new("schedule.yaml"), &expected)
                .is_err()
        );
        assert_eq!(std::fs::read(&target).unwrap(), b"replacement");
    }

    #[cfg(unix)]
    #[test]
    fn directory_lock_same_pin_acquisitions_are_independent_after_path_replacement() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("node");
        std::fs::create_dir(&path).unwrap();
        let directory = PinnedDirectory::open(&path).unwrap().unwrap();
        let guard = directory.lock_exclusive().unwrap();
        let retained = guard.clone();
        assert!(
            directory
                .lock_exclusive_with_timeout(crate::time::Duration::ZERO)
                .is_err()
        );
        drop(guard);
        assert!(
            directory
                .lock_exclusive_with_timeout(crate::time::Duration::ZERO)
                .is_err()
        );
        std::fs::rename(&path, root.path().join("original")).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(
            directory
                .lock_exclusive_with_timeout(crate::time::Duration::ZERO)
                .is_err()
        );
        drop(retained);
        let acquired = directory
            .lock_exclusive_with_timeout(crate::time::Duration::ZERO)
            .unwrap();
        acquired.ensure_protects(&directory).unwrap();
        assert!(
            acquired
                .ensure_protects(&PinnedDirectory::open(&path).unwrap().unwrap())
                .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn inherited_directory_lock_requires_owned_description_and_preserves_exclusion() {
        let temp = tempfile::tempdir().unwrap();
        let directory = PinnedDirectory::open(temp.path()).unwrap().unwrap();
        let unowned = directory.try_clone_descriptor().unwrap();
        assert!(
            directory
                .require_inherited_exclusive_lock(unowned.as_raw_fd() as u32)
                .is_err()
        );
        let guard = directory.lock_exclusive().unwrap();
        let coordinate = guard.inner.file.as_raw_fd() as u32;
        directory
            .require_inherited_exclusive_lock(coordinate)
            .unwrap();
        assert!(
            directory
                .require_inherited_exclusive_lock(unowned.as_raw_fd() as u32)
                .is_err()
        );
        let other_temp = tempfile::tempdir().unwrap();
        let other = PinnedDirectory::open(other_temp.path()).unwrap().unwrap();
        assert!(other.require_inherited_exclusive_lock(coordinate).is_err());
        assert!(
            directory
                .lock_exclusive_with_timeout(std::time::Duration::ZERO)
                .is_err()
        );
        drop(guard);
        directory
            .lock_exclusive_with_timeout(std::time::Duration::ZERO)
            .unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn inherited_directory_lock_survives_exec_and_releases_on_exit() {
        const PHASE: &str = "LILLUX_LOCK_EXEC_TEST_PHASE";
        const ROOT: &str = "LILLUX_LOCK_EXEC_TEST_ROOT";
        const FD: &str = "LILLUX_LOCK_EXEC_TEST_FD";
        const TEST: &str =
            "secure_fs::tests::inherited_directory_lock_survives_exec_and_releases_on_exit";
        match std::env::var(PHASE).ok().as_deref() {
            Some("before") => {
                let directory = PinnedDirectory::open(Path::new(&std::env::var(ROOT).unwrap()))
                    .unwrap()
                    .unwrap();
                let guard = directory.lock_exclusive().unwrap();
                let mut command = std::process::Command::new(std::env::current_exe().unwrap());
                command
                    .args(["--exact", TEST, "--nocapture"])
                    .env(PHASE, "after");
                match guard.exec_command_with_lock(&mut command, FD) {
                    Ok(never) => match never {},
                    Err(error) => panic!("exec child while retaining lock: {error:#}"),
                }
            }
            Some("after") => {
                let directory = PinnedDirectory::open(Path::new(&std::env::var(ROOT).unwrap()))
                    .unwrap()
                    .unwrap();
                directory
                    .require_inherited_exclusive_lock(std::env::var(FD).unwrap().parse().unwrap())
                    .unwrap();
                assert!(
                    directory
                        .lock_exclusive_with_timeout(std::time::Duration::ZERO)
                        .is_err()
                );
            }
            None => {
                let temp = tempfile::tempdir().unwrap();
                let status = std::process::Command::new(std::env::current_exe().unwrap())
                    .args(["--exact", TEST, "--nocapture"])
                    .env(PHASE, "before")
                    .env(ROOT, temp.path())
                    .status()
                    .unwrap();
                assert!(status.success());
                let directory = PinnedDirectory::open(temp.path()).unwrap().unwrap();
                directory
                    .lock_exclusive_with_timeout(std::time::Duration::ZERO)
                    .unwrap();
            }
            Some(other) => panic!("unexpected fixture phase {other}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn cloned_directory_lock_releases_only_after_last_guard() {
        let dir = tempfile::tempdir().unwrap();
        let directory = PinnedDirectory::open(dir.path()).unwrap().unwrap();
        let contender = PinnedDirectory::open(dir.path()).unwrap().unwrap();
        let guard = directory.lock_exclusive().unwrap();
        let retained = guard.clone();
        retained.ensure_protects(&directory).unwrap();
        drop(guard);

        assert_eq!(
            unsafe {
                libc::flock(
                    contender.directory.as_raw_fd(),
                    libc::LOCK_EX | libc::LOCK_NB,
                )
            },
            -1
        );
        assert_eq!(
            std::io::Error::last_os_error().kind(),
            std::io::ErrorKind::WouldBlock
        );

        drop(retained);
        assert_eq!(
            unsafe {
                libc::flock(
                    contender.directory.as_raw_fd(),
                    libc::LOCK_EX | libc::LOCK_NB,
                )
            },
            0
        );
        unsafe {
            libc::flock(contender.directory.as_raw_fd(), libc::LOCK_UN);
        }
    }

    #[cfg(unix)]
    #[test]
    fn directory_lock_rejects_a_different_inode() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        let first = PinnedDirectory::open(first.path()).unwrap().unwrap();
        let second = PinnedDirectory::open(second.path()).unwrap().unwrap();
        let guard = first.lock_exclusive().unwrap();

        assert!(guard.ensure_protects(&second).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn timed_directory_lock_is_bounded_and_leaves_no_namespace_entry() {
        let dir = tempfile::tempdir().unwrap();
        let holder = PinnedDirectory::open(dir.path()).unwrap().unwrap();
        let contender = PinnedDirectory::open(dir.path()).unwrap().unwrap();
        let before = contender.entry_names().unwrap();
        let guard = holder.lock_exclusive().unwrap();

        let error = contender
            .lock_exclusive_with_timeout(crate::time::Duration::from_millis(20))
            .unwrap_err();
        assert!(error.to_string().contains("timed out"));
        assert_eq!(contender.entry_names().unwrap(), before);

        drop(guard);
        let acquired = contender
            .lock_exclusive_with_timeout(crate::time::Duration::from_millis(100))
            .unwrap();
        acquired.ensure_protects(&contender).unwrap();
        drop(acquired);
        assert_eq!(contender.entry_names().unwrap(), before);
    }

    #[cfg(unix)]
    #[test]
    fn absent_conditional_publication_never_replaces_an_existing_name() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("cas-entry");
        std::fs::write(&target, b"existing").unwrap();
        let directory = PinnedDirectory::open(dir.path()).unwrap().unwrap();

        assert!(
            directory
                .atomic_write_if_same(OsStr::new("cas-entry"), None, b"replacement", 0o600)
                .is_err()
        );
        assert_eq!(std::fs::read(&target).unwrap(), b"existing");
    }

    #[cfg(unix)]
    #[test]
    fn streamed_atomic_create_can_retain_typed_file_authority() {
        let dir = tempfile::tempdir().unwrap();
        let directory = PinnedDirectory::open(dir.path()).unwrap().unwrap();
        let mut payload = b"pinned payload".as_slice();

        let (pinned, copied) = directory
            .atomic_create_pinned_regular_from_reader(
                OsStr::new("payload"),
                &mut payload,
                64,
                0o600,
            )
            .unwrap()
            .unwrap();

        assert_eq!(copied, 14);
        assert_eq!(pinned.name(), OsStr::new("payload"));
        assert_eq!(pinned.path(), dir.path().join("payload"));
        assert_eq!(pinned.read_bounded(64).unwrap(), b"pinned payload");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn pinned_link_is_create_only_and_accepts_only_the_same_inode() {
        let source = tempfile::tempdir().unwrap();
        let destination = tempfile::tempdir().unwrap();
        std::fs::write(source.path().join("payload"), b"immutable").unwrap();
        let source = PinnedDirectory::open(source.path()).unwrap().unwrap();
        let destination = PinnedDirectory::open(destination.path()).unwrap().unwrap();
        let payload = source
            .open_regular(OsStr::new("payload"), false)
            .unwrap()
            .unwrap();

        destination
            .link_regular_from(
                OsStr::new("linked"),
                &source,
                OsStr::new("payload"),
                &payload,
            )
            .unwrap();
        destination
            .link_regular_from(
                OsStr::new("linked"),
                &source,
                OsStr::new("payload"),
                &payload,
            )
            .unwrap();
        std::fs::write(destination.path().join("conflict"), b"different").unwrap();
        assert!(
            destination
                .link_regular_from(
                    OsStr::new("conflict"),
                    &source,
                    OsStr::new("payload"),
                    &payload,
                )
                .is_err()
        );
        assert_eq!(
            std::fs::read(destination.path().join("linked")).unwrap(),
            b"immutable"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn regular_noreplace_rename_moves_exact_inode_without_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("quarantine"), b"preserve").unwrap();
        let directory = PinnedDirectory::open(dir.path()).unwrap().unwrap();
        let quarantine = directory
            .open_regular(OsStr::new("quarantine"), false)
            .unwrap()
            .unwrap();

        directory
            .rename_regular_child_noreplace_atomic(
                OsStr::new("quarantine"),
                OsStr::new("recovered"),
                &quarantine,
            )
            .unwrap();
        assert!(!dir.path().join("quarantine").exists());
        assert_eq!(
            std::fs::read(dir.path().join("recovered")).unwrap(),
            b"preserve"
        );

        std::fs::write(dir.path().join("next"), b"next").unwrap();
        let next = directory
            .open_regular(OsStr::new("next"), false)
            .unwrap()
            .unwrap();
        assert!(
            directory
                .rename_regular_child_noreplace_atomic(
                    OsStr::new("next"),
                    OsStr::new("recovered"),
                    &next,
                )
                .is_err()
        );
        assert_eq!(std::fs::read(dir.path().join("next")).unwrap(), b"next");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn bounded_entry_enumeration_never_materializes_more_than_the_limit() {
        let dir = tempfile::tempdir().unwrap();
        for index in 0..10 {
            std::fs::write(dir.path().join(format!("entry-{index}")), b"value").unwrap();
        }
        let directory = PinnedDirectory::open(dir.path()).unwrap().unwrap();

        let names = directory.entry_names_bounded(3).unwrap();

        assert_eq!(names.len(), 3);
        assert!(names.windows(2).all(|pair| pair[0] <= pair[1]));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn bounded_regular_file_walk_accepts_the_exact_total_and_depth_boundary() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("nested")).unwrap();
        std::fs::write(dir.path().join("root"), b"root").unwrap();
        std::fs::write(dir.path().join("nested/leaf"), b"leaf").unwrap();
        let pinned = PinnedDirectory::open(dir.path()).unwrap().unwrap();
        let mut visited = Vec::new();

        pinned
            .visit_regular_files_bounded(
                DirectoryTraversalBudget::new(3, 1),
                |_relative, _directory| Ok(false),
                |relative, _file| {
                    visited.push(relative.to_path_buf());
                    Ok(())
                },
            )
            .unwrap();

        visited.sort();
        assert_eq!(
            visited,
            vec![PathBuf::from("nested/leaf"), PathBuf::from("root")]
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn secure_tree_copy_is_descriptor_relative_bounded_and_filtered() {
        use std::os::unix::fs::PermissionsExt as _;

        let source = tempfile::tempdir().unwrap();
        let destination = tempfile::tempdir().unwrap();
        std::fs::create_dir(source.path().join("nested")).unwrap();
        std::fs::create_dir(source.path().join(".venv")).unwrap();
        std::fs::write(source.path().join("nested/tool"), b"exact").unwrap();
        std::fs::set_permissions(
            source.path().join("nested/tool"),
            std::fs::Permissions::from_mode(0o750),
        )
        .unwrap();
        std::fs::write(source.path().join(".venv/ambient"), b"excluded").unwrap();
        let source = PinnedDirectory::open(source.path()).unwrap().unwrap();
        let destination = PinnedDirectory::open(destination.path()).unwrap().unwrap();

        source
            .copy_contents_to_filtered(
                &destination,
                DirectoryTraversalBudget::new(8, 4),
                |relative| Ok(relative == Path::new(".venv")),
            )
            .unwrap();

        assert_eq!(
            std::fs::read(destination.path().join("nested/tool")).unwrap(),
            b"exact"
        );
        assert_eq!(
            std::fs::metadata(destination.path().join("nested/tool"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o750
        );
        assert!(!destination.path().join(".venv").exists());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn bounded_regular_file_walk_counts_nested_and_pruned_entries_before_callbacks() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["a", "b", "c"] {
            std::fs::create_dir(dir.path().join(name)).unwrap();
        }
        let pinned = PinnedDirectory::open(dir.path()).unwrap().unwrap();
        let mut callbacks = 0_usize;

        let error = pinned
            .visit_regular_files_bounded(
                DirectoryTraversalBudget::new(2, 8),
                |_relative, _directory| {
                    callbacks += 1;
                    Ok(true)
                },
                |_relative, _file| Ok(()),
            )
            .unwrap_err()
            .to_string();

        assert!(error.contains("maximum entry count 2"), "{error}");
        assert_eq!(callbacks, 0, "overflow must be rejected before callbacks");

        std::fs::remove_dir_all(dir.path().join("c")).unwrap();
        std::fs::write(dir.path().join("a/leaf"), b"leaf").unwrap();
        let error = pinned
            .visit_regular_files_bounded(
                DirectoryTraversalBudget::new(2, 8),
                |_relative, _directory| Ok(false),
                |_relative, _file| Ok(()),
            )
            .unwrap_err()
            .to_string();
        assert!(error.contains("maximum entry count 0"), "{error}");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn bounded_regular_file_walk_rejects_depth_symlinks_and_special_entries() {
        use std::os::unix::ffi::OsStrExt as _;
        use std::os::unix::fs::symlink;

        let deep = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(deep.path().join("one/two")).unwrap();
        std::fs::write(deep.path().join("one/two/value"), b"value").unwrap();
        let pinned = PinnedDirectory::open(deep.path()).unwrap().unwrap();
        let error = pinned
            .visit_regular_files_bounded(
                DirectoryTraversalBudget::new(8, 1),
                |_relative, _directory| Ok(false),
                |_relative, _file| Ok(()),
            )
            .unwrap_err()
            .to_string();
        assert!(error.contains("maximum depth 1"), "{error}");

        let unsupported = tempfile::tempdir().unwrap();
        std::fs::write(unsupported.path().join("target"), b"value").unwrap();
        symlink("target", unsupported.path().join("link")).unwrap();
        let pinned = PinnedDirectory::open(unsupported.path()).unwrap().unwrap();
        assert!(
            pinned
                .visit_regular_files_bounded(
                    DirectoryTraversalBudget::new(8, 1),
                    |_relative, _directory| Ok(false),
                    |_relative, _file| Ok(()),
                )
                .is_err()
        );

        std::fs::remove_file(unsupported.path().join("link")).unwrap();
        let fifo =
            std::ffi::CString::new(unsupported.path().join("pipe").as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        let pinned = PinnedDirectory::open(unsupported.path()).unwrap().unwrap();
        assert!(
            pinned
                .visit_regular_files_bounded(
                    DirectoryTraversalBudget::new(8, 1),
                    |_relative, _directory| Ok(false),
                    |_relative, _file| Ok(()),
                )
                .is_err()
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn bounded_regular_file_walk_reports_a_missing_root_without_callbacks() {
        let root = std::env::temp_dir().join(format!(
            "lillux-missing-walk-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        let callbacks = std::cell::Cell::new(0_usize);
        let present = visit_regular_files_no_follow_bounded(
            &root,
            DirectoryTraversalBudget::new(1, 1),
            |_relative, _directory| {
                callbacks.set(callbacks.get() + 1);
                Ok(false)
            },
            |_relative, _file| {
                callbacks.set(callbacks.get() + 1);
                Ok(())
            },
        )
        .unwrap();
        assert!(!present);
        assert_eq!(callbacks.get(), 0);
    }

    #[test]
    fn exact_bounded_open_read_enforces_length_and_limit_before_body_growth() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("value");
        std::fs::write(&path, b"exact").unwrap();

        let exact = std::fs::File::open(&path).unwrap();
        assert_eq!(
            read_open_regular_file_exact_bounded(exact, 5, 5).unwrap(),
            b"exact"
        );

        let wrong_observation = std::fs::File::open(&path).unwrap();
        assert!(read_open_regular_file_exact_bounded(wrong_observation, 4, 5).is_err());

        let over_limit = std::fs::File::open(&path).unwrap();
        assert!(read_open_regular_file_exact_bounded(over_limit, 5, 4).is_err());

        let before_growth = std::fs::metadata(&path).unwrap().len();
        std::fs::write(&path, b"exact-and-grown").unwrap();
        let grown = std::fs::File::open(&path).unwrap();
        assert!(read_open_regular_file_exact_bounded(grown, before_growth, 64).is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn prepared_batch_entry_is_hidden_until_its_complete_bytes_are_flushed() {
        let dir = tempfile::tempdir().unwrap();
        let directory = PinnedDirectory::open(dir.path()).unwrap().unwrap();
        let prepared = directory
            .prepare_atomic_create(OsStr::new("cas-entry"), b"complete bytes", 0o600)
            .unwrap()
            .unwrap();

        assert!(!dir.path().join("cas-entry").exists());
        directory.sync_filesystem().unwrap();
        assert!(prepared.publish().unwrap());
        directory.sync_filesystem().unwrap();
        assert_eq!(
            std::fs::read(dir.path().join("cas-entry")).unwrap(),
            b"complete bytes"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn tree_sync_remains_bound_to_the_pinned_directory_after_namespace_replacement() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let original = root.path().join("generation");
        let displaced = root.path().join("displaced");
        std::fs::create_dir_all(original.join("nested")).unwrap();
        std::fs::write(original.join("nested/value"), b"immutable").unwrap();
        let pinned = PinnedDirectory::open(&original).unwrap().unwrap();

        std::fs::rename(&original, &displaced).unwrap();
        std::fs::create_dir(&original).unwrap();
        symlink("missing", original.join("replacement-link")).unwrap();

        pinned.sync_tree().unwrap();
        assert_eq!(
            std::fs::read(displaced.join("nested/value")).unwrap(),
            b"immutable"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn tree_sync_rejects_symlink_entries() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("value"), b"value").unwrap();
        symlink("value", root.path().join("linked")).unwrap();
        let pinned = PinnedDirectory::open(root.path()).unwrap().unwrap();

        let error = pinned.sync_tree().unwrap_err().to_string();
        assert!(
            error.contains("rejected a symlink or non-regular entry"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn private_file_materialization_is_exact_and_independent() {
        let root = tempfile::tempdir().unwrap();
        let source_path = root.path().join("source");
        let target_path = root.path().join("target");
        std::fs::write(&source_path, b"admitted bytes").unwrap();
        std::fs::create_dir(&target_path).unwrap();
        let source = std::fs::File::open(&source_path).unwrap();
        let target = PinnedDirectory::open(&target_path).unwrap().unwrap();
        let mut copy_budget = 64;

        let outcome = target
            .materialize_private_regular_child(
                OsStr::new("value"),
                &source,
                14,
                0o644,
                &mut copy_budget,
            )
            .unwrap();
        assert_eq!(
            std::fs::read(target_path.join("value")).unwrap(),
            b"admitted bytes"
        );
        match outcome {
            PrivateFileMaterialization::Reflink => assert_eq!(copy_budget, 64),
            PrivateFileMaterialization::Copied => assert_eq!(copy_budget, 50),
        }

        std::fs::write(target_path.join("value"), b"child mutation").unwrap();
        assert_eq!(std::fs::read(source_path).unwrap(), b"admitted bytes");
    }

    #[cfg(unix)]
    #[test]
    fn owner_private_tree_tightening_never_follows_opaque_links() {
        use std::os::unix::fs::{PermissionsExt as _, symlink};

        let parent = tempfile::tempdir().unwrap();
        let home_path = parent.path().join("home");
        let nested = home_path.join("nested");
        let state = nested.join("state");
        let outside = parent.path().join("outside");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(&state, b"state").unwrap();
        std::fs::write(&outside, b"outside").unwrap();
        std::fs::set_permissions(&home_path, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::set_permissions(&nested, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o644)).unwrap();
        std::fs::set_permissions(&outside, std::fs::Permissions::from_mode(0o644)).unwrap();
        symlink("../outside", home_path.join("link")).unwrap();

        let home = PinnedDirectory::open(&home_path).unwrap().unwrap();
        assert_eq!(
            home.tighten_owner_private_tree_bounded(DirectoryTraversalBudget::new(3, 1), 5,)
                .unwrap(),
            5
        );
        assert_eq!(
            std::fs::metadata(&home_path).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(&nested).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(&state).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(&outside).unwrap().permissions().mode() & 0o777,
            0o644
        );
        assert!(
            std::fs::symlink_metadata(home_path.join("link"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            home.require_owner_private_tree_bounded(DirectoryTraversalBudget::new(3, 1), 5,)
                .unwrap(),
            5
        );
    }

    #[cfg(unix)]
    #[test]
    fn live_owner_private_directory_protection_does_not_snapshot_mutable_children() {
        use std::os::unix::fs::PermissionsExt as _;

        let root = tempfile::tempdir().unwrap();
        let child = root.path().join("workload-state");
        std::fs::write(&child, b"mutable").unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::set_permissions(&child, std::fs::Permissions::from_mode(0o644)).unwrap();

        let pinned = PinnedDirectory::open(root.path()).unwrap().unwrap();
        pinned.tighten_owner_private_directory().unwrap();

        assert_eq!(
            std::fs::metadata(root.path()).unwrap().permissions().mode() & 0o7777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(&child).unwrap().permissions().mode() & 0o777,
            0o644
        );
    }

    #[cfg(unix)]
    #[test]
    fn owner_private_tree_validation_rejects_non_private_permissions() {
        use std::os::unix::fs::PermissionsExt as _;

        let root = tempfile::tempdir().unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let state = root.path().join("state");
        std::fs::write(&state, b"state").unwrap();
        std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o604)).unwrap();
        let pinned = PinnedDirectory::open(root.path()).unwrap().unwrap();
        assert!(
            pinned
                .require_owner_private_tree_bounded(DirectoryTraversalBudget::new(1, 1), 5,)
                .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn owner_enclosed_tree_accepts_opaque_descendant_modes_but_not_a_public_root() {
        use std::os::unix::fs::PermissionsExt as _;

        let root = tempfile::tempdir().unwrap();
        let nested = root.path().join("nested");
        let state = nested.join("installation_id");
        std::fs::create_dir(&nested).unwrap();
        std::fs::write(&state, b"state").unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::set_permissions(&nested, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o644)).unwrap();

        let pinned = PinnedDirectory::open(root.path()).unwrap().unwrap();
        assert_eq!(
            pinned
                .require_owner_enclosed_tree_bounded(DirectoryTraversalBudget::new(2, 1), 5,)
                .unwrap(),
            5
        );
        assert_eq!(
            std::fs::metadata(&state).unwrap().permissions().mode() & 0o777,
            0o644
        );

        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o750)).unwrap();
        assert!(
            pinned
                .require_owner_enclosed_tree_bounded(DirectoryTraversalBudget::new(2, 1), 5,)
                .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn owner_private_tree_rejects_regular_hard_links() {
        use std::os::unix::fs::PermissionsExt as _;

        let parent = tempfile::tempdir().unwrap();
        let home_path = parent.path().join("home");
        let outside = parent.path().join("outside");
        std::fs::create_dir(&home_path).unwrap();
        std::fs::set_permissions(&home_path, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::write(&outside, b"outside").unwrap();
        std::fs::set_permissions(&outside, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::hard_link(&outside, home_path.join("linked")).unwrap();
        let home = PinnedDirectory::open(&home_path).unwrap().unwrap();

        assert!(
            home.tighten_owner_private_tree_bounded(DirectoryTraversalBudget::new(1, 1), 7,)
                .is_err()
        );
        assert_eq!(
            std::fs::metadata(&outside).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
