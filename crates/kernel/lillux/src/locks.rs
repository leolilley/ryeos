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
struct FileLockGuard {
    #[cfg(unix)]
    _file: fs::File,
    #[cfg(unix)]
    parent: fs::File,
    #[cfg(unix)]
    target_name: CString,
}

#[derive(Clone, Copy)]
enum FileLockMode {
    Shared,
    Exclusive,
}

pub struct ExclusiveFileLock {
    _guard: FileLockGuard,
}

/// A shared interprocess lock held until the value is dropped.
///
/// Readers use this to retain one stable filesystem generation while still
/// allowing unrelated readers to proceed. Writers taking
/// [`ExclusiveFileLock`] remain serialized against every shared holder.
pub struct SharedFileLock {
    _guard: FileLockGuard,
}

impl ExclusiveFileLock {
    pub fn acquire(target: &Path) -> Result<Self> {
        FileLockGuard::acquire_inner(target, true, FileLockMode::Exclusive)
            .map(|_guard| Self { _guard })
    }

    /// Acquire with a bounded wait and diagnostic owner evidence. This is for
    /// availability-critical startup boundaries where parking indefinitely is
    /// worse than refusing with the kernel-reported holder PID.
    pub fn acquire_with_timeout(target: &Path, timeout: std::time::Duration) -> Result<Self> {
        #[cfg(unix)]
        {
            let parent_path = target.parent().unwrap_or_else(|| Path::new("."));
            let parent = open_directory_no_follow(parent_path, true)?;
            let file_name = target
                .file_name()
                .ok_or_else(|| anyhow::anyhow!("lock target has no file name"))?;
            FileLockGuard::acquire_with_parent_timeout(
                parent,
                file_name,
                true,
                FileLockMode::Exclusive,
                timeout,
            )
            .map(|_guard| Self { _guard })
        }
        #[cfg(not(unix))]
        {
            let _ = (target, timeout);
            anyhow::bail!("interprocess file locking is unavailable on this platform")
        }
    }

    /// Acquire an already-established lock anchor without creating any
    /// directory or file. Read-only/dry-run callers use this to obtain the
    /// same consistent snapshot as writers without mutating the filesystem.
    pub fn acquire_existing(target: &Path) -> Result<Self> {
        FileLockGuard::acquire_inner(target, false, FileLockMode::Exclusive)
            .map(|_guard| Self { _guard })
    }

    /// Acquire the persistent lock anchor relative to one exact pinned parent
    /// directory. Namespace-enumerating callers use this instead of resolving
    /// the parent's ordinary pathname a second time.
    pub fn acquire_in(
        parent: &crate::secure_fs::PinnedDirectory,
        target_name: &OsStr,
    ) -> Result<Self> {
        FileLockGuard::acquire_in_inner(parent, target_name, true, FileLockMode::Exclusive)
            .map(|_guard| Self { _guard })
    }

    /// Acquire a lock anchor relative to one exact pinned parent, with a
    /// bounded wait and kernel holder diagnostics. The anchor name is the
    /// normal `.{target_name}.lock` used by the other file-lock constructors.
    pub fn acquire_in_with_timeout(
        parent: &crate::secure_fs::PinnedDirectory,
        target_name: &OsStr,
        timeout: std::time::Duration,
    ) -> Result<Self> {
        #[cfg(unix)]
        {
            FileLockGuard::acquire_with_parent_timeout(
                parent.try_clone_descriptor()?,
                target_name,
                true,
                FileLockMode::Exclusive,
                timeout,
            )
            .map(|_guard| Self { _guard })
        }
        #[cfg(not(unix))]
        {
            let _ = (parent, target_name, timeout);
            anyhow::bail!("interprocess file locking is unavailable on this platform")
        }
    }

    /// Acquire an already-established lock anchor relative to one exact pinned
    /// parent without creating filesystem state.
    pub fn acquire_existing_in(
        parent: &crate::secure_fs::PinnedDirectory,
        target_name: &OsStr,
    ) -> Result<Self> {
        FileLockGuard::acquire_in_inner(parent, target_name, false, FileLockMode::Exclusive)
            .map(|_guard| Self { _guard })
    }

    pub fn open_target_read(&self) -> Result<fs::File> {
        self._guard.open_target_read()
    }

    pub fn open_target_append_create(&self) -> Result<fs::File> {
        self._guard.open_target_append_create()
    }

    pub fn sync_parent(&self) -> Result<()> {
        self._guard.sync_parent()
    }

    pub fn replace_target(&self, data: &[u8]) -> Result<()> {
        self._guard.replace_target(data)
    }
}

impl SharedFileLock {
    pub fn acquire(target: &Path) -> Result<Self> {
        FileLockGuard::acquire_inner(target, true, FileLockMode::Shared)
            .map(|_guard| Self { _guard })
    }

    pub fn acquire_existing(target: &Path) -> Result<Self> {
        FileLockGuard::acquire_inner(target, false, FileLockMode::Shared)
            .map(|_guard| Self { _guard })
    }

    pub fn acquire_in(
        parent: &crate::secure_fs::PinnedDirectory,
        target_name: &OsStr,
    ) -> Result<Self> {
        FileLockGuard::acquire_in_inner(parent, target_name, true, FileLockMode::Shared)
            .map(|_guard| Self { _guard })
    }

    pub fn acquire_existing_in(
        parent: &crate::secure_fs::PinnedDirectory,
        target_name: &OsStr,
    ) -> Result<Self> {
        FileLockGuard::acquire_in_inner(parent, target_name, false, FileLockMode::Shared)
            .map(|_guard| Self { _guard })
    }
}

impl FileLockGuard {
    fn acquire_in_inner(
        parent: &crate::secure_fs::PinnedDirectory,
        target_name: &OsStr,
        create_missing: bool,
        mode: FileLockMode,
    ) -> Result<Self> {
        #[cfg(unix)]
        {
            Self::acquire_with_parent(
                parent.try_clone_descriptor()?,
                target_name,
                create_missing,
                mode,
            )
        }
        #[cfg(not(unix))]
        {
            let _ = (parent, target_name, create_missing, mode);
            anyhow::bail!("interprocess file locking is unavailable on this platform")
        }
    }

    fn acquire_inner(target: &Path, create_missing: bool, mode: FileLockMode) -> Result<Self> {
        #[cfg(unix)]
        {
            let parent_path = target.parent().unwrap_or_else(|| Path::new("."));
            let parent = open_directory_no_follow(parent_path, create_missing)?;
            let file_name = target
                .file_name()
                .ok_or_else(|| anyhow::anyhow!("lock target has no file name"))?;
            Self::acquire_with_parent(parent, file_name, create_missing, mode)
        }
        #[cfg(not(unix))]
        {
            let _ = (target, create_missing, mode);
            anyhow::bail!("interprocess file locking is unavailable on this platform")
        }
    }

    #[cfg(unix)]
    fn acquire_with_parent(
        parent: fs::File,
        target_name: &OsStr,
        create_missing: bool,
        mode: FileLockMode,
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
        let access_flag = match mode {
            FileLockMode::Shared => libc::O_RDONLY,
            FileLockMode::Exclusive => libc::O_RDWR,
        };
        let file_fd = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                lock_name.as_ptr(),
                access_flag | create_flag | libc::O_CLOEXEC | libc::O_NOFOLLOW,
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
        let operation = match mode {
            FileLockMode::Shared => libc::LOCK_SH,
            FileLockMode::Exclusive => libc::LOCK_EX,
        };
        if unsafe { libc::flock(file.as_raw_fd(), operation) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(Self {
            _file: file,
            parent,
            target_name: target_name_c,
        })
    }

    #[cfg(unix)]
    fn acquire_with_parent_timeout(
        parent: fs::File,
        target_name: &OsStr,
        create_missing: bool,
        mode: FileLockMode,
        timeout: std::time::Duration,
    ) -> Result<Self> {
        use std::os::fd::{AsRawFd, FromRawFd};
        use std::os::unix::ffi::OsStrExt as _;

        let target_bytes = target_name.as_bytes();
        if target_bytes.is_empty()
            || target_bytes.contains(&b'/')
            || target_bytes == b"."
            || target_bytes == b".."
        {
            anyhow::bail!("lock target must be one safe child name");
        }
        let target_name_c = CString::new(target_bytes)?;
        let lock_name =
            CString::new([b".".as_slice(), target_bytes, b".lock".as_slice()].concat())?;
        let create_flag = if create_missing { libc::O_CREAT } else { 0 };
        let access_flag = match mode {
            FileLockMode::Shared => libc::O_RDONLY,
            FileLockMode::Exclusive => libc::O_RDWR,
        };
        let descriptor = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                lock_name.as_ptr(),
                access_flag | create_flag | libc::O_CLOEXEC | libc::O_NOFOLLOW,
                0o600,
            )
        };
        if descriptor < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let file = unsafe { fs::File::from_raw_fd(descriptor) };
        if !file.metadata()?.file_type().is_file() {
            anyhow::bail!("lock anchor is not a regular file");
        }
        let operation = match mode {
            FileLockMode::Shared => libc::LOCK_SH,
            FileLockMode::Exclusive => libc::LOCK_EX,
        } | libc::LOCK_NB;
        let started = std::time::Instant::now();
        loop {
            if unsafe { libc::flock(file.as_raw_fd(), operation) } == 0 {
                break;
            }
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::WouldBlock {
                return Err(error.into());
            }
            if started.elapsed() >= timeout {
                let holder = linux_flock_holder_pid(&file)
                    .map(|pid| pid.to_string())
                    .unwrap_or_else(|| "unknown".to_string());
                anyhow::bail!(
                    "timed out after {:.1}s waiting for lock {} (holder pid: {holder})",
                    timeout.as_secs_f64(),
                    String::from_utf8_lossy(target_bytes)
                );
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        Ok(Self {
            _file: file,
            parent,
            target_name: target_name_c,
        })
    }

    /// Open the locked target without following a final-component symlink.
    fn open_target_read(&self) -> Result<fs::File> {
        #[cfg(unix)]
        {
            self.open_target(libc::O_RDONLY)
        }
        #[cfg(not(unix))]
        anyhow::bail!("interprocess file locking is unavailable on this platform")
    }

    /// Open or create the locked target for append without following a
    /// final-component symlink.
    fn open_target_append_create(&self) -> Result<fs::File> {
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
    fn sync_parent(&self) -> Result<()> {
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
    fn replace_target(&self, data: &[u8]) -> Result<()> {
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

#[cfg(target_os = "linux")]
fn linux_flock_holder_pid(file: &fs::File) -> Option<i64> {
    use std::os::unix::fs::MetadataExt as _;
    let metadata = file.metadata().ok()?;
    let major = libc::major(metadata.dev());
    let minor = libc::minor(metadata.dev());
    let needle = format!("{major:02x}:{minor:02x}:{}", metadata.ino());
    let locks = fs::read_to_string("/proc/locks").ok()?;
    locks.lines().find_map(|line| {
        let fields = line.split_whitespace().collect::<Vec<_>>();
        (fields.len() >= 6 && fields[1] == "FLOCK" && fields[5] == needle)
            .then(|| fields[4].parse::<i64>().ok())
            .flatten()
    })
}

#[cfg(all(unix, not(target_os = "linux")))]
fn linux_flock_holder_pid(_file: &fs::File) -> Option<i64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn shared_locks_coexist_and_exclude_a_writer() {
        use std::sync::mpsc;
        use std::time::Duration;

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        let first_reader = SharedFileLock::acquire(&target).unwrap();
        let second_reader = SharedFileLock::acquire_existing(&target).unwrap();

        let (acquired_tx, acquired_rx) = mpsc::channel();
        let writer_target = target.clone();
        let writer = std::thread::spawn(move || {
            let _writer = ExclusiveFileLock::acquire_existing(&writer_target).unwrap();
            acquired_tx.send(()).unwrap();
        });

        assert!(acquired_rx
            .recv_timeout(Duration::from_millis(100))
            .is_err());
        drop(first_reader);
        assert!(acquired_rx
            .recv_timeout(Duration::from_millis(100))
            .is_err());
        drop(second_reader);
        acquired_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        writer.join().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn shared_existing_lock_only_requires_read_access_to_anchor() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        drop(ExclusiveFileLock::acquire(&target).unwrap());
        let anchor = dir.path().join(".target.lock");
        fs::set_permissions(&anchor, fs::Permissions::from_mode(0o400)).unwrap();

        drop(SharedFileLock::acquire_existing(&target).unwrap());
    }

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
