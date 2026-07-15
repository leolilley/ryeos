//! Descriptor-relative, no-follow reads and deterministic directory walks.
//!
//! These helpers are for authoritative inputs whose trust must not be rebound
//! by swapping a symlink or ancestor between a pathname check and open.

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

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

/// One regular entry opened from a [`PinnedDirectory`].
pub struct PinnedRegularFile {
    pub path: PathBuf,
    pub name: OsString,
    pub file: File,
}

/// RAII advisory lock for a pinned directory inode. Directory-scoped writers
/// use this without introducing lock-anchor files into closed namespaces.
#[derive(Debug)]
pub struct PinnedDirectoryLock {
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

impl Drop for PinnedDirectoryLock {
    fn drop(&mut self) {
        #[cfg(unix)]
        unsafe {
            libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

impl PinnedDirectory {
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

    /// Linux descriptor-rooted pathname for this exact directory inode.
    /// This is a narrow bridge for legacy APIs that accept only a root path;
    /// replacing the directory's ordinary pathname cannot redirect them.
    pub fn descriptor_path(&self) -> Result<PathBuf> {
        #[cfg(not(target_os = "linux"))]
        anyhow::bail!("descriptor-rooted paths are unavailable on this platform");
        #[cfg(target_os = "linux")]
        {
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
    pub(crate) fn try_clone_descriptor(&self) -> Result<File> {
        self.directory
            .try_clone()
            .with_context(|| format!("duplicate pinned directory {}", self.path.display()))
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
            Ok(PinnedDirectoryLock { file })
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
        #[cfg(not(unix))]
        {
            let _ = (name, expected);
            anyhow::bail!("secure file removal is unavailable on this platform");
        }
        #[cfg(unix)]
        {
            validate_child_name(name)?;
            let name_c = std::ffi::CString::new(name.as_bytes())?;
            ensure_entry_matches(
                &self.directory,
                &name_c,
                Some(expected),
                &self.path.join(name),
            )?;
            if unsafe { libc::unlinkat(self.directory.as_raw_fd(), name_c.as_ptr(), 0) } != 0 {
                return Err(std::io::Error::last_os_error()).with_context(|| {
                    format!("remove secure file {}", self.path.join(name).display())
                });
            }
            self.directory.sync_all()?;
            Ok(())
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
            let current_metadata = current.directory.metadata()?;
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
