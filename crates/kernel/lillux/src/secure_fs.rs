//! Descriptor-relative, no-follow reads and deterministic directory walks.
//!
//! These helpers are for authoritative inputs whose trust must not be rebound
//! by swapping a symlink or ancestor between a pathname check and open.

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};

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
    pub device_id: u64,
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

/// One regular entry opened from a [`PinnedDirectory`].
pub struct PinnedRegularFile {
    pub path: PathBuf,
    pub name: OsString,
    pub file: File,
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
            &mut prune,
            &mut visit,
        )
    }

    /// Enumerate immediate children from the pinned directory descriptor and
    /// classify each entry without following links.
    #[cfg(unix)]
    pub fn entries_no_follow(&self) -> Result<Vec<PinnedDirectoryEntryMetadata>> {
        let mut entries = Vec::new();
        for name in directory_names_bounded(&self.directory, None)? {
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
            });
        }
        entries.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(entries)
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

    /// Serialize cooperating mutations of this exact directory namespace.
    pub fn lock_exclusive(&self) -> Result<PinnedDirectoryLock> {
        #[cfg(not(unix))]
        anyhow::bail!("pinned directory locking is unavailable on this platform");
        #[cfg(unix)]
        {
            let file = self.directory.try_clone()?;
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
                return Err(std::io::Error::last_os_error())
                    .with_context(|| format!("lock pinned directory {}", self.path.display()));
            }
            Ok(PinnedDirectoryLock {
                inner: Arc::new(PinnedDirectoryLockInner { file }),
            })
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
                            "conditional replace refused an unexpected target; it remains preserved as {} because restoration raced: {error:#}; {restore:#}", self.path.join(&quarantine_name).display()
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
                            "conditional replace refused changed target content; it remains preserved as {} because restoration raced: {error:#}; {restore:#}", self.path.join(&quarantine_name).display()
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
                            "conditional replace did not publish; the verified prior target remains preserved as {} because restoration raced: {error:#}; {restore:#}", self.path.join(&quarantine_name).display()
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
        #[cfg(not(unix))]
        anyhow::bail!("secure directory enumeration is unavailable on this platform");
        #[cfg(unix)]
        {
            let mut entries = Vec::new();
            for name in directory_names(&self.directory)? {
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
                            "conditional remove refused an unexpected target; it remains preserved as {} because restoration raced: {error:#}; {restore:#}", self.path.join(&quarantine_name).display()
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
                            "conditional remove refused changed target content; it remains preserved as {} because restoration raced: {error:#}; {restore:#}", self.path.join(&quarantine_name).display()
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
            let current =
                open_child_directory(&self.directory, &name_c, &path)?.ok_or_else(|| {
                    anyhow::anyhow!("secure child directory disappeared: {}", path.display())
                })?;
            let current_metadata = current.metadata()?;
            let expected_metadata = expected.directory.metadata()?;
            if current_metadata.dev() != expected_metadata.dev()
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

#[cfg(all(unix, not(target_os = "linux")))]
fn directory_names(_directory: &File) -> Result<Vec<std::ffi::OsString>> {
    anyhow::bail!("secure descriptor-relative directory walking is unavailable on this platform")
}

/// Open and read an existing regular file without following any path
/// component. Missing files are errors; callers with optional semantics should
/// establish absence separately from their typed namespace contract.
pub fn read_regular_file_no_follow(path: &Path) -> Result<Vec<u8>> {
    #[cfg(not(unix))]
    {
        let _ = path;
        anyhow::bail!("secure no-follow file reading is unavailable on this platform");
    }
    #[cfg(unix)]
    {
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        let directory = open_directory_no_follow(parent)?.ok_or_else(|| {
            anyhow::anyhow!("secure file parent does not exist: {}", parent.display())
        })?;
        let name = path
            .file_name()
            .ok_or_else(|| anyhow::anyhow!("secure file path has no filename"))?;
        let name = std::ffi::CString::new(name.as_bytes())?;
        let mut file = open_regular_at(&directory, &name, path)?
            .ok_or_else(|| anyhow::anyhow!("secure file does not exist: {}", path.display()))?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        Ok(bytes)
    }
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
        visit_from_open_directory(root, Path::new(""), &directory, &mut prune, &mut visit)?;
        Ok(true)
    }
}

#[cfg(unix)]
fn visit_from_open_directory<P, V>(
    root: &Path,
    relative_directory: &Path,
    directory: &File,
    prune: &mut P,
    visit: &mut V,
) -> Result<()>
where
    P: FnMut(&Path, bool) -> Result<bool>,
    V: FnMut(&Path, File) -> Result<()>,
{
    for name in directory_names(directory)? {
        let relative = relative_directory.join(&name);
        let display = root.join(&relative);
        let name_c = std::ffi::CString::new(name.as_bytes())?;
        if let Some(child_directory) = open_child_directory(directory, &name_c, &display)? {
            if !prune(&relative, true)? {
                visit_from_open_directory(root, &relative, &child_directory, prune, visit)?;
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

        assert!(destination
            .replace_regular_from_if_matches_atomic(
                OsStr::new("value"),
                Some(&expected),
                |_| Ok(()),
                &source,
                OsStr::new("staged"),
                &staged,
            )
            .is_err());
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

        assert!(directory
            .remove_if_same_atomic(OsStr::new("value"), &expected)
            .is_err());
        assert_eq!(
            std::fs::read(root.path().join("value")).unwrap(),
            b"local edit"
        );
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

        assert!(directory
            .atomic_write_if_same(
                OsStr::new("schedule.yaml"),
                Some(&expected),
                b"desired",
                0o600,
            )
            .is_err());
        assert!(directory
            .remove_if_same(OsStr::new("schedule.yaml"), &expected)
            .is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"replacement");
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
    fn absent_conditional_publication_never_replaces_an_existing_name() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("cas-entry");
        std::fs::write(&target, b"existing").unwrap();
        let directory = PinnedDirectory::open(dir.path()).unwrap().unwrap();

        assert!(directory
            .atomic_write_if_same(OsStr::new("cas-entry"), None, b"replacement", 0o600)
            .is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"existing");
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
        assert!(destination
            .link_regular_from(
                OsStr::new("conflict"),
                &source,
                OsStr::new("payload"),
                &payload,
            )
            .is_err());
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
        assert!(directory
            .rename_regular_child_noreplace_atomic(
                OsStr::new("next"),
                OsStr::new("recovered"),
                &next,
            )
            .is_err());
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
}
