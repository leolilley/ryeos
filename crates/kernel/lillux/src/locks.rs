use std::ffi::OsStr;
use std::path::Path;

use anyhow::Result;

#[cfg(unix)]
use std::ffi::CString;
use std::fs;
#[cfg(unix)]
use std::io::Write;

#[cfg(unix)]
fn open_directory_no_follow(path: &Path, create_missing: bool) -> Result<fs::File> {
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::ffi::OsStrExt;
    use std::path::Component;

    let start = if path.is_absolute() { "/" } else { "." };
    let start = CString::new(start).expect("static path contains no NUL");
    let descriptor = unsafe {
        libc::open(
            start.as_ptr(),
            libc::O_RDONLY
                | libc::O_DIRECTORY
                | libc::O_CLOEXEC
                | libc::O_NOFOLLOW
                | libc::O_NONBLOCK,
        )
    };
    if descriptor < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let mut directory = unsafe { fs::File::from_raw_fd(descriptor) };
    for component in path.components() {
        let component = match component {
            Component::RootDir | Component::CurDir => continue,
            Component::Normal(component) => component,
            Component::ParentDir | Component::Prefix(_) => {
                anyhow::bail!("lock path contains an unsafe parent component")
            }
        };
        let component = CString::new(component.as_bytes())?;
        let mut descriptor = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                component.as_ptr(),
                libc::O_RDONLY
                    | libc::O_DIRECTORY
                    | libc::O_CLOEXEC
                    | libc::O_NOFOLLOW
                    | libc::O_NONBLOCK,
            )
        };
        if descriptor < 0 {
            let error = std::io::Error::last_os_error();
            if !create_missing || error.kind() != std::io::ErrorKind::NotFound {
                return Err(error.into());
            }
            if unsafe { libc::mkdirat(directory.as_raw_fd(), component.as_ptr(), 0o777) } != 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() != std::io::ErrorKind::AlreadyExists {
                    return Err(error.into());
                }
            }
            descriptor = unsafe {
                libc::openat(
                    directory.as_raw_fd(),
                    component.as_ptr(),
                    libc::O_RDONLY
                        | libc::O_DIRECTORY
                        | libc::O_CLOEXEC
                        | libc::O_NOFOLLOW
                        | libc::O_NONBLOCK,
                )
            };
            if descriptor < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            directory.sync_all()?;
        }
        directory = unsafe { fs::File::from_raw_fd(descriptor) };
    }
    Ok(directory)
}

/// Hold an interprocess lock associated with `target` for the entire operation.
/// Callers performing read-modify-write must place the read inside this scope.
pub fn with_exclusive_file_lock<T>(
    target: &Path,
    operation: impl FnOnce() -> Result<T>,
) -> Result<T> {
    #[cfg(unix)]
    {
        let lock = ExclusiveFileLock::acquire(target)?;
        let result = operation();
        drop(lock);
        result
    }

    #[cfg(not(unix))]
    {
        let _ = (target, operation);
        anyhow::bail!("interprocess file locking is unavailable on this platform")
    }
}

/// An interprocess lock held until the value is dropped.
///
/// Use this instead of [`with_exclusive_file_lock`] when one logical mutation
/// spans async work or several independently fallible phases.
pub struct ExclusiveFileLock {
    #[cfg(unix)]
    _file: fs::File,
    #[cfg(unix)]
    parent: fs::File,
    #[cfg(unix)]
    target_name: CString,
}

impl ExclusiveFileLock {
    pub fn acquire(target: &Path) -> Result<Self> {
        Self::acquire_inner(target, true)
    }

    /// Acquire an already-established lock anchor without creating any
    /// directory or file. Read-only/dry-run callers use this to obtain the
    /// same consistent snapshot as writers without mutating the filesystem.
    pub fn acquire_existing(target: &Path) -> Result<Self> {
        Self::acquire_inner(target, false)
    }

    /// Acquire the persistent lock anchor relative to one exact pinned parent
    /// directory. Namespace-enumerating callers use this instead of resolving
    /// the parent's ordinary pathname a second time.
    pub fn acquire_in(
        parent: &crate::secure_fs::PinnedDirectory,
        target_name: &OsStr,
    ) -> Result<Self> {
        Self::acquire_in_inner(parent, target_name, true)
    }

    /// Acquire an already-established lock anchor relative to one exact pinned
    /// parent without creating filesystem state.
    pub fn acquire_existing_in(
        parent: &crate::secure_fs::PinnedDirectory,
        target_name: &OsStr,
    ) -> Result<Self> {
        Self::acquire_in_inner(parent, target_name, false)
    }

    fn acquire_in_inner(
        parent: &crate::secure_fs::PinnedDirectory,
        target_name: &OsStr,
        create_missing: bool,
    ) -> Result<Self> {
        #[cfg(unix)]
        {
            Self::acquire_with_parent(parent.try_clone_descriptor()?, target_name, create_missing)
        }
        #[cfg(not(unix))]
        {
            let _ = (parent, target_name, create_missing);
            anyhow::bail!("interprocess file locking is unavailable on this platform")
        }
    }

    fn acquire_inner(target: &Path, create_missing: bool) -> Result<Self> {
        #[cfg(unix)]
        {
            let parent_path = target.parent().unwrap_or_else(|| Path::new("."));
            let parent = open_directory_no_follow(parent_path, create_missing)?;
            let file_name = target
                .file_name()
                .ok_or_else(|| anyhow::anyhow!("lock target has no file name"))?;
            Self::acquire_with_parent(parent, file_name, create_missing)
        }
        #[cfg(not(unix))]
        {
            let _ = (target, create_missing);
            anyhow::bail!("interprocess file locking is unavailable on this platform")
        }
    }

    #[cfg(unix)]
    fn acquire_with_parent(
        parent: fs::File,
        target_name: &OsStr,
        create_missing: bool,
    ) -> Result<Self> {
        use std::os::fd::{AsRawFd, FromRawFd};
        use std::os::unix::ffi::OsStrExt;

        let target_bytes = target_name.as_bytes();
        if target_bytes.is_empty()
            || target_bytes.contains(&b'/')
            || target_bytes == b"."
            || target_bytes == b".."
        {
            anyhow::bail!("lock target must be one safe child name");
        }
        let target_name_c = CString::new(target_name.as_bytes())?;
        let mut lock_name = Vec::with_capacity(target_name.as_bytes().len() + 6);
        lock_name.extend_from_slice(b".");
        lock_name.extend_from_slice(target_name.as_bytes());
        lock_name.extend_from_slice(b".lock");
        let lock_name = CString::new(lock_name)?;
        let create_flag = if create_missing { libc::O_CREAT } else { 0 };
        let file_fd = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                lock_name.as_ptr(),
                libc::O_RDWR | create_flag | libc::O_CLOEXEC | libc::O_NOFOLLOW,
                0o600,
            )
        };
        if file_fd < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let file = unsafe { fs::File::from_raw_fd(file_fd) };
        if !file.metadata()?.file_type().is_file() {
            anyhow::bail!("lock anchor is not a regular file");
        }
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(Self {
            _file: file,
            parent,
            target_name: target_name_c,
        })
    }

    /// Open the locked target without following a final-component symlink.
    pub fn open_target_read(&self) -> Result<fs::File> {
        #[cfg(unix)]
        {
            self.open_target(libc::O_RDONLY)
        }
        #[cfg(not(unix))]
        anyhow::bail!("interprocess file locking is unavailable on this platform")
    }

    /// Open or create the locked target for append without following a
    /// final-component symlink.
    pub fn open_target_append_create(&self) -> Result<fs::File> {
        #[cfg(unix)]
        {
            self.open_target(libc::O_RDWR | libc::O_APPEND | libc::O_CREAT)
        }
        #[cfg(not(unix))]
        anyhow::bail!("interprocess file locking is unavailable on this platform")
    }

    #[cfg(unix)]
    fn open_target(&self, flags: libc::c_int) -> Result<fs::File> {
        use std::os::fd::{AsRawFd, FromRawFd};

        let fd = unsafe {
            libc::openat(
                self.parent.as_raw_fd(),
                self.target_name.as_ptr(),
                flags | libc::O_CLOEXEC | libc::O_NOFOLLOW,
                0o600,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let file = unsafe { fs::File::from_raw_fd(fd) };
        if !file.metadata()?.file_type().is_file() {
            anyhow::bail!("locked target is not a regular file");
        }
        Ok(file)
    }

    /// Sync the directory that names the target. This is required after the
    /// first create and after atomic replacement.
    pub fn sync_parent(&self) -> Result<()> {
        #[cfg(unix)]
        {
            self.parent.sync_all()?;
            Ok(())
        }
        #[cfg(not(unix))]
        anyhow::bail!("directory durability is unavailable on this platform")
    }

    /// Atomically replace the locked target relative to the already-open,
    /// non-symlink parent directory, then make the namespace change durable.
    pub fn replace_target(&self, data: &[u8]) -> Result<()> {
        #[cfg(unix)]
        {
            use std::os::fd::{AsRawFd, FromRawFd};

            let sequence = crate::atomic_fs::next_temp_sequence();
            let temp_name =
                CString::new(format!(".fires.tmp.{}.{}", std::process::id(), sequence))?;
            let fd = unsafe {
                libc::openat(
                    self.parent.as_raw_fd(),
                    temp_name.as_ptr(),
                    libc::O_WRONLY
                        | libc::O_CREAT
                        | libc::O_EXCL
                        | libc::O_CLOEXEC
                        | libc::O_NOFOLLOW,
                    0o600,
                )
            };
            if fd < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            let mut temp = unsafe { fs::File::from_raw_fd(fd) };
            let result = (|| -> Result<()> {
                temp.write_all(data)?;
                temp.sync_all()?;
                if unsafe {
                    libc::renameat(
                        self.parent.as_raw_fd(),
                        temp_name.as_ptr(),
                        self.parent.as_raw_fd(),
                        self.target_name.as_ptr(),
                    )
                } != 0
                {
                    return Err(std::io::Error::last_os_error().into());
                }
                self.sync_parent()
            })();
            if result.is_err() {
                unsafe {
                    libc::unlinkat(self.parent.as_raw_fd(), temp_name.as_ptr(), 0);
                }
            }
            result
        }
        #[cfg(not(unix))]
        {
            let _ = data;
            anyhow::bail!("atomic locked replacement is unavailable on this platform")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acquire_existing_never_creates_lock_anchor() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        fs::write(&target, b"value").unwrap();
        assert!(ExclusiveFileLock::acquire_existing(&target).is_err());
        assert!(!dir.path().join(".target.lock").exists());

        drop(ExclusiveFileLock::acquire(&target).unwrap());
        let before = fs::read_dir(dir.path()).unwrap().count();
        drop(ExclusiveFileLock::acquire_existing(&target).unwrap());
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), before);
    }

    #[cfg(unix)]
    #[test]
    fn lock_rejects_symlink_as_final_parent() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let real_parent = dir.path().join("real");
        fs::create_dir(&real_parent).unwrap();
        let linked_parent = dir.path().join("linked");
        symlink(&real_parent, &linked_parent).unwrap();

        let result = ExclusiveFileLock::acquire(&linked_parent.join("target"));

        assert!(result.is_err());
        assert!(!real_parent.join(".target.lock").exists());
    }

    #[cfg(unix)]
    #[test]
    fn lock_rejects_symlink_in_ancestor_path() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let outside = dir.path().join("outside");
        fs::create_dir(&outside).unwrap();
        let linked = dir.path().join("linked");
        symlink(&outside, &linked).unwrap();

        let result = ExclusiveFileLock::acquire(&linked.join("nested/target"));

        assert!(result.is_err());
        assert!(!outside.join("nested").exists());
    }
}
