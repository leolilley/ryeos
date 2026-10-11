use std::ffi::OsStr;
use std::path::Path;

use anyhow::{Context as _, Result};

#[cfg(unix)]
use std::ffi::CString;
use std::fs;
#[cfg(unix)]
use std::io::Write;

/// Lock an already-open regular file in shared mode until the caller's existing
/// deadline. This consumes and returns the SAME descriptor: it never opens a
/// path, creates a sibling anchor, or writes lock contents. The caller must
/// supply a fresh independently owned file, not an alias of an existing lock.
/// Keep the caller's fork-sensitive descriptor lease through this acquisition
/// and the returned file's lifetime. Contention is bounded; filesystem metadata
/// inspection is not an interruptible operation.
pub fn acquire_existing_shared_file_until(
    file: fs::File,
    deadline: crate::time::MonotonicDeadline,
) -> std::io::Result<fs::File> {
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;

        let expired = || {
            std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "existing shared file lock deadline elapsed",
            )
        };
        if deadline.has_elapsed() {
            return Err(expired());
        }
        if !file.metadata()?.is_file() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "existing shared file lock requires a regular file",
            ));
        }
        loop {
            if deadline.has_elapsed() {
                return Err(expired());
            }
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) } == 0 {
                // A successful native call after expiry supplies no authority.
                // Dropping this uniquely owned file releases its acquired lock.
                if deadline.has_elapsed() {
                    return Err(expired());
                }
                return Ok(file);
            }
            let error = std::io::Error::last_os_error();
            match error.kind() {
                std::io::ErrorKind::Interrupted => continue,
                std::io::ErrorKind::WouldBlock => {
                    let remaining = deadline.remaining();
                    if remaining.is_zero() {
                        return Err(expired());
                    }
                    crate::time::sleep(remaining.min(crate::time::Duration::from_millis(10)));
                }
                _ => return Err(error),
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (file, deadline);
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "interprocess file locking is unavailable on this platform",
        ))
    }
}

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

/// Non-blocking exclusive lease on the exact regular file named by `target`.
///
/// This differs from [`ExclusiveFileLock`], whose hidden sibling anchor
/// serializes mutations *of* a target. An `ExactExclusiveFileLock` locks the
/// target inode itself, retains its descriptor and parent authority, and can
/// later prove that an expected pathname still selects that exact lock. It is
/// used for long-lived operational exclusion files such as a node's
/// `operator.lock`.
pub struct ExactExclusiveFileLock {
    process: u32,
    #[cfg(unix)]
    file: crate::exec::ForkChildCloseFile,
    #[cfg(not(unix))]
    file: fs::File,
    parent: fs::File,
    target_name: CString,
}

/// Refusal of an exact operational lock because another open file
/// description holds it, as distinct from every other acquisition failure
/// (an absent lock file, a wrong file type, an I/O error).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExactLockHeld {
    /// The holder's pid, or `unknown` when it cannot be read.
    pub holder: String,
    /// The bounded wait spent before refusing, if any.
    pub waited: Option<std::time::Duration>,
}
impl std::fmt::Display for ExactLockHeld {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let wait = self
            .waited
            .map(|timeout| format!(" after waiting {:.1}s", timeout.as_secs_f64()))
            .unwrap_or_default();
        write!(
            formatter,
            "state lock held by another process (pid: {}){wait}; stop the daemon or other standalone service before proceeding",
            self.holder
        )
    }
}
impl std::error::Error for ExactLockHeld {}

/// Identities observed from the SAME retained target and parent descriptors.
/// This copyable data supplies neither a lock grant nor a close receipt.
#[derive(Clone, Copy, Debug)]
pub struct ExactExclusiveFileLockObservation {
    target: crate::OpenFileIdentity,
    parent: crate::OpenFileIdentity,
}
impl ExactExclusiveFileLockObservation {
    pub fn target(&self) -> crate::OpenFileIdentity { self.target }
    pub fn parent(&self) -> crate::OpenFileIdentity { self.parent }
}

impl ExactExclusiveFileLock {
    /// Reject copied process-local custody before touching retained descriptor
    /// coordinates or the fork-child registration. This issues no lock grant.
    pub fn require_original_process(&self) -> Result<()> {
        anyhow::ensure!(self.process == std::process::id(),
            "exact operational lock belongs to another process incarnation");
        Ok(())
    }
    /// Observe the actual retained descriptors without reopening, duplicating
    /// or exporting them. The original controller funds both observations
    /// before this call. Returned identities are evidence only, never authority.
    #[cfg(target_os = "linux")]
    pub fn observe_retained_descriptors(&self) -> Result<ExactExclusiveFileLockObservation> {
        self.require_original_process()?;
        anyhow::ensure!(self.file.is_registered_live(), "operator target no longer live");
        anyhow::ensure!(self.file.metadata()?.is_file() && self.parent.metadata()?.is_dir(),
            "original operator descriptor roles changed");
        Ok(ExactExclusiveFileLockObservation {
            target: crate::observe_open_file_identity(&self.file)?,
            parent: crate::observe_open_file_identity(&self.parent)?,
        })
    }

    /// Open or create `target`, acquire `LOCK_EX | LOCK_NB`, and publish the
    /// current PID as diagnostic content only after the lease is held.
    pub fn acquire(target: &Path) -> Result<Self> {
        Self::acquire_inner(target, true, None)
    }

    /// Acquire the exact operational lock with a bounded wait.
    ///
    /// Crash-supervised processes use this while a killed predecessor's
    /// pre-exec descendants finish kernel teardown. The same descriptor-pinned
    /// pathname checks and exclusive flock apply throughout; expiry never
    /// weakens mutual exclusion.
    pub fn acquire_with_timeout(target: &Path, timeout: std::time::Duration) -> Result<Self> {
        Self::acquire_inner(target, true, Some(timeout))
    }

    /// Acquire an already-existing target without creating or modifying it.
    pub fn acquire_existing_read_only(target: &Path) -> Result<Self> {
        Self::acquire_inner(target, false, None)
    }

    fn acquire_inner(
        target: &Path,
        create_missing: bool,
        wait_timeout: Option<std::time::Duration>,
    ) -> Result<Self> {
        #[cfg(not(unix))]
        {
            let _ = (target, create_missing, wait_timeout);
            anyhow::bail!("exact operational file locking is unavailable on this platform")
        }
        #[cfg(unix)]
        {
            use std::io::{Seek as _, Write as _};
            use std::os::fd::{AsRawFd as _, FromRawFd as _};
            use std::os::unix::ffi::OsStrExt as _;

            // Retain the transient barrier from before opening the descriptor
            // until its long-lived child-close registration is visible. A
            // direct-attachment fork can therefore never inherit the lock in
            // the gap between those operations.
            let fork_sensitive_descriptors = crate::exec::retain_fork_sensitive_descriptors();
            let parent_path = target.parent().unwrap_or_else(|| Path::new("."));
            let parent = open_directory_no_follow(parent_path, create_missing)?;
            let target_name = target
                .file_name()
                .ok_or_else(|| anyhow::anyhow!("operational lock target has no file name"))?;
            validate_lock_target_name(target_name)?;
            let target_name = CString::new(target_name.as_bytes())?;
            let access = if create_missing {
                libc::O_RDWR
            } else {
                libc::O_RDONLY
            };
            let mut created = false;
            let mut descriptor = -1;
            if create_missing {
                descriptor = unsafe {
                    libc::openat(
                        parent.as_raw_fd(),
                        target_name.as_ptr(),
                        access | libc::O_CREAT | libc::O_EXCL | libc::O_CLOEXEC | libc::O_NOFOLLOW,
                        0o600,
                    )
                };
                if descriptor >= 0 {
                    created = true;
                } else {
                    let error = std::io::Error::last_os_error();
                    if error.kind() != std::io::ErrorKind::AlreadyExists {
                        return Err(error).with_context(|| {
                            format!("create exact operational lock {}", target.display())
                        });
                    }
                }
            }
            if descriptor < 0 {
                descriptor = unsafe {
                    libc::openat(
                        parent.as_raw_fd(),
                        target_name.as_ptr(),
                        access | libc::O_CLOEXEC | libc::O_NOFOLLOW,
                        0,
                    )
                };
            }
            if descriptor < 0 {
                return Err(std::io::Error::last_os_error())
                    .with_context(|| format!("open exact operational lock {}", target.display()));
            }
            let mut file = unsafe { fs::File::from_raw_fd(descriptor) };
            if !file.metadata()?.file_type().is_file() {
                anyhow::bail!(
                    "exact operational lock is not a regular file: {}",
                    target.display()
                );
            }
            let started = std::time::Instant::now();
            loop {
                if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                    break;
                }
                let error = std::io::Error::last_os_error();
                if error.kind() != std::io::ErrorKind::WouldBlock
                    && error.raw_os_error() != Some(libc::EWOULDBLOCK)
                {
                    return Err(error).with_context(|| {
                        format!("acquire exact operational lock {}", target.display())
                    });
                }
                if let Some(timeout) = wait_timeout
                    && started.elapsed() < timeout
                {
                    std::thread::sleep(std::time::Duration::from_millis(25));
                    continue;
                }
                let holder = linux_flock_holder_pid(&file)
                    .map(|pid| pid.to_string())
                    .unwrap_or_else(|| "unknown".to_string());
                return Err(ExactLockHeld { holder, waited: wait_timeout }.into());
            }
            if create_missing {
                file.set_len(0)?;
                file.rewind()?;
                writeln!(&mut file, "{}", std::process::id())?;
                file.sync_all()?;
                if created {
                    parent.sync_all()?;
                }
            }
            let file = crate::exec::register_fork_child_close_file(file);
            drop(fork_sensitive_descriptors);
            Ok(Self {
                process: std::process::id(),
                file,
                parent,
                target_name,
            })
        }
    }

    /// Close the actual registered lock target once, under its original fork
    /// barrier. A busy barrier or expired original deadline retains the file.
    /// The caller must finish durable account work before releasing exclusion.
    /// Success is physical close evidence only, including a late observation;
    /// it grants no admission, settlement or renewed cleanup authority.
    #[cfg(target_os = "linux")]
    pub fn try_retire_registered_target(
        &mut self,
        original: crate::time::MonotonicDeadline,
    ) -> Result<bool> {
        self.require_original_process()?;
        self.file.try_retire_registered(original).map_err(anyhow::Error::msg)
    }

    /// Move the SAME retained parent after successful explicit target close.
    /// A refusal returns the complete lock body, including failed-close facts.
    /// The caller must still physically retire this parent under its original
    /// funded controller; moving it does not produce a parent-close receipt.
    #[cfg(target_os = "linux")]
    pub fn into_retired_target_parent(self) -> std::result::Result<fs::File, (Self, anyhow::Error)> {
        if let Err(error) = self.require_original_process() {
            return Err((self, error));
        }
        if !self.file.explicitly_retired() {
            return Err((self, anyhow::anyhow!("registered lock target has no successful close observation")));
        }
        Ok(self.parent)
    }

    /// Prove `target` still resolves to the exact parent/name/inode held by
    /// this lease. No path component is followed.
    pub fn ensure_path_binding(&self, target: &Path) -> Result<()> {
        self.require_original_process()?;
        #[cfg(target_os = "linux")]
        anyhow::ensure!(self.file.is_registered_live(), "operational lock target no longer live");
        #[cfg(not(unix))]
        {
            let _ = target;
            anyhow::bail!("exact operational lock binding is unavailable on this platform")
        }
        #[cfg(unix)]
        {
            use std::os::fd::{AsRawFd as _, FromRawFd as _};
            use std::os::unix::ffi::OsStrExt as _;
            use std::os::unix::fs::MetadataExt as _;

            let expected_name = target
                .file_name()
                .ok_or_else(|| anyhow::anyhow!("operational lock target has no file name"))?;
            if expected_name.as_bytes() != self.target_name.as_bytes() {
                anyhow::bail!("operational lock protects a different child name");
            }
            let parent_path = target.parent().unwrap_or_else(|| Path::new("."));
            let expected_parent = open_directory_no_follow(parent_path, false)?;
            let held_parent = self.parent.metadata()?;
            let observed_parent = expected_parent.metadata()?;
            if held_parent.dev() != observed_parent.dev()
                || held_parent.ino() != observed_parent.ino()
            {
                anyhow::bail!("operational lock protects a different parent directory");
            }
            let descriptor = unsafe {
                libc::openat(
                    self.parent.as_raw_fd(),
                    self.target_name.as_ptr(),
                    libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
                    0,
                )
            };
            if descriptor < 0 {
                return Err(std::io::Error::last_os_error())
                    .context("reopen exact operational lock binding");
            }
            let observed = unsafe { fs::File::from_raw_fd(descriptor) };
            let held = self.file.metadata()?;
            let observed = observed.metadata()?;
            if !observed.file_type().is_file()
                || held.dev() != observed.dev()
                || held.ino() != observed.ino()
            {
                anyhow::bail!("operational lock pathname changed while the lease was held");
            }
            Ok(())
        }
    }
}

#[cfg(unix)]
fn validate_lock_target_name(name: &OsStr) -> Result<()> {
    use std::os::unix::ffi::OsStrExt as _;
    let bytes = name.as_bytes();
    if bytes.is_empty() || bytes.contains(&b'/') || bytes == b"." || bytes == b".." {
        anyhow::bail!("operational lock target must be one safe child name");
    }
    Ok(())
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

    /// Acquire a shared lock with a bounded wait and holder diagnostics.
    /// Read-only composition uses this at availability-critical standalone
    /// boundaries while remaining compatible with other generation readers.
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
                FileLockMode::Shared,
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
pub(crate) fn linux_flock_holder_pid(file: &fs::File) -> Option<i64> {
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
pub(crate) fn linux_flock_holder_pid(_file: &fs::File) -> Option<i64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    // Host close controls only: no native admission or account settlement proof.
    #[cfg(target_os = "linux")]
    #[test]
    fn registered_target_close_refusal_preserves_exclusion_then_releases_exact_target() {
        use std::os::unix::fs::MetadataExt as _;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("operator.lock");
        let mut lock = ExactExclusiveFileLock::acquire(&path).unwrap();
        let parent_identity = lock.parent.metadata().unwrap();
        let original_observation = lock.observe_retained_descriptors().unwrap();
        assert_eq!(original_observation.parent().inode(), parent_identity.ino());
        assert_eq!(original_observation.target().inode(), lock.file.metadata().unwrap().ino());
        assert!(!lock.try_retire_registered_target(
            crate::time::MonotonicDeadline::after(std::time::Duration::ZERO),
        ).unwrap());
        lock.ensure_path_binding(&path).unwrap();
        assert!(ExactExclusiveFileLock::acquire(&path).is_err());
        assert!(lock.try_retire_registered_target(
            crate::time::MonotonicDeadline::after(std::time::Duration::from_secs(1)),
        ).unwrap());
        assert!(lock.ensure_path_binding(&path).is_err());
        let replacement = ExactExclusiveFileLock::acquire(&path).unwrap();
        // Cached physical success must not close the newly acquired descriptor.
        assert!(lock.try_retire_registered_target(
            crate::time::MonotonicDeadline::after(std::time::Duration::ZERO),
        ).unwrap());
        replacement.ensure_path_binding(&path).unwrap();
        assert!(ExactExclusiveFileLock::acquire(&path).is_err());
        let parent = match lock.into_retired_target_parent() {
            Ok(parent) => parent,
            Err(_) => panic!("successful registered close must retain its exact parent"),
        };
        let observed = parent.metadata().unwrap();
        assert_eq!((observed.dev(), observed.ino()), (parent_identity.dev(), parent_identity.ino()));
    }

    #[cfg(unix)]
    #[test]
    fn exact_shared_file_until_preserves_descriptor_inode_and_bytes() {
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::MetadataExt;

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("cas-mutation.lock");
        fs::write(&target, b"original lock bytes").unwrap();
        let file = fs::File::open(&target).unwrap();
        let original_fd = file.as_raw_fd();
        let original_inode = file.metadata().unwrap().ino();
        // The native primitive follows the supplied descriptor, not this path.
        let retained = dir.path().join("original");
        fs::rename(&target, &retained).unwrap();
        fs::write(&target, b"replacement").unwrap();
        let held = acquire_existing_shared_file_until(
            file,
            crate::time::MonotonicDeadline::after(crate::time::Duration::from_secs(1)),
        )
        .unwrap();
        assert_eq!(held.as_raw_fd(), original_fd);
        assert_eq!(held.metadata().unwrap().ino(), original_inode);
        assert_eq!(fs::read(&retained).unwrap(), b"original lock bytes");
        assert_eq!(fs::read(&target).unwrap(), b"replacement");
        assert!(!dir.path().join(".cas-mutation.lock.lock").exists());
        let contender = fs::File::open(&retained).unwrap();
        assert_ne!(
            unsafe { libc::flock(contender.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        drop(held);
        assert_eq!(
            unsafe { libc::flock(contender.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
    }

    #[cfg(unix)]
    #[test]
    fn exact_shared_file_until_refuses_contention_and_releases_after_owner_drop() {
        use std::os::fd::AsRawFd;

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("cas-mutation.lock");
        fs::write(&target, b"unchanged").unwrap();
        let owner = fs::File::open(&target).unwrap();
        assert_eq!(
            unsafe { libc::flock(owner.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        let deadline =
            crate::time::MonotonicDeadline::after(crate::time::Duration::from_millis(20));
        let error = acquire_existing_shared_file_until(fs::File::open(&target).unwrap(), deadline)
            .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        assert!(deadline.has_elapsed());
        assert_eq!(fs::read(&target).unwrap(), b"unchanged");
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
        drop(owner);
        let held = acquire_existing_shared_file_until(
            fs::File::open(&target).unwrap(),
            crate::time::MonotonicDeadline::after(crate::time::Duration::from_secs(1)),
        )
        .unwrap();
        drop(held);
        let contender = fs::File::open(&target).unwrap();
        assert_eq!(
            unsafe { libc::flock(contender.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
    }

    #[cfg(unix)]
    #[test]
    fn exact_shared_file_until_refuses_expired_without_locking() {
        use std::os::fd::AsRawFd;

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("lock");
        fs::write(&target, b"unchanged").unwrap();
        let error = acquire_existing_shared_file_until(
            fs::File::open(&target).unwrap(),
            crate::time::MonotonicDeadline::after(crate::time::Duration::ZERO),
        )
        .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        let contender = fs::File::open(&target).unwrap();
        assert_eq!(
            unsafe { libc::flock(contender.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        assert_eq!(fs::read(&target).unwrap(), b"unchanged");
    }

    #[cfg(unix)]
    #[test]
    fn exact_shared_file_until_refuses_nonregular_file() {
        let dir = tempfile::tempdir().unwrap();
        let error = acquire_existing_shared_file_until(
            fs::File::open(dir.path()).unwrap(),
            crate::time::MonotonicDeadline::after(crate::time::Duration::from_secs(1)),
        )
        .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
    }

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

        assert!(
            acquired_rx
                .recv_timeout(Duration::from_millis(100))
                .is_err()
        );
        drop(first_reader);
        assert!(
            acquired_rx
                .recv_timeout(Duration::from_millis(100))
                .is_err()
        );
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
