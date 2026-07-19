use std::fs;
use std::io::ErrorKind;
use std::path::Path;

#[cfg(unix)]
use std::io::Write;
#[cfg(unix)]
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::Result;

/// Failure phase for an atomic namespace mutation.
#[derive(Debug)]
pub enum AtomicMutationError {
    /// The target namespace was not changed by this operation.
    BeforeCommit(anyhow::Error),
    /// Rename, exchange, or removal completed, but the following durability
    /// barrier failed. The new namespace is observable but crash durability is
    /// uncertain.
    DurabilityUncertain(anyhow::Error),
    /// A conditional mutation refused to publish the requested value, but a
    /// racing namespace prevented restoration of the quarantined prior entry.
    /// The unexpected and prior entries remain preserved for explicit
    /// recovery; the requested mutation itself did not commit.
    NamespaceChanged(anyhow::Error),
}

impl AtomicMutationError {
    pub(crate) fn before(error: impl Into<anyhow::Error>) -> Self {
        Self::BeforeCommit(error.into())
    }

    pub(crate) fn durability(error: impl Into<anyhow::Error>) -> Self {
        Self::DurabilityUncertain(error.into())
    }

    pub(crate) fn namespace_changed(error: impl Into<anyhow::Error>) -> Self {
        Self::NamespaceChanged(error.into())
    }

    pub fn namespace_committed(&self) -> bool {
        matches!(self, Self::DurabilityUncertain(_))
    }

    pub fn namespace_requires_recovery(&self) -> bool {
        matches!(self, Self::NamespaceChanged(_))
    }
}

impl std::fmt::Display for AtomicMutationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BeforeCommit(error) => {
                write!(formatter, "atomic mutation did not commit: {error:#}")
            }
            Self::DurabilityUncertain(error) => write!(
                formatter,
                "atomic mutation committed but durability is uncertain: {error:#}"
            ),
            Self::NamespaceChanged(error) => write!(
                formatter,
                "atomic mutation did not publish the requested value but namespace recovery is required: {error:#}"
            ),
        }
    }
}

impl std::error::Error for AtomicMutationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::BeforeCommit(error)
            | Self::DurabilityUncertain(error)
            | Self::NamespaceChanged(error) => Some(error.root_cause()),
        }
    }
}

pub type AtomicMutationResult<T> = std::result::Result<T, AtomicMutationError>;

#[cfg(unix)]
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[cfg(unix)]
pub(crate) fn next_temp_sequence() -> u64 {
    TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
}

pub fn atomic_write(target: &Path, data: &[u8]) -> AtomicMutationResult<()> {
    #[cfg(unix)]
    {
        atomic_write_unix(target, data, 0o666)
    }
    #[cfg(not(unix))]
    {
        let _ = (target, data);
        unsupported_mutation("durable atomic write")
    }
}

/// Atomically replace `target`, applying `mode` before the temp file is
/// published into the namespace.
pub fn atomic_write_with_mode(target: &Path, data: &[u8], mode: u32) -> AtomicMutationResult<()> {
    #[cfg(unix)]
    {
        atomic_write_unix(target, data, mode)
    }
    #[cfg(not(unix))]
    {
        let _ = (target, data, mode);
        unsupported_mutation("mode-aware durable atomic write")
    }
}

/// Atomically replace `target` with private data.
///
/// On Unix the temporary file is created as `0600` before any bytes are
/// written, so secret material is never briefly exposed under a permissive
/// mode. The file and containing directory are synced before success returns.
pub fn atomic_write_private(target: &Path, data: &[u8]) -> AtomicMutationResult<()> {
    #[cfg(unix)]
    {
        atomic_write_unix(target, data, 0o600)
    }
    #[cfg(not(unix))]
    {
        let _ = (target, data);
        unsupported_mutation("private durable atomic write")
    }
}

/// Remove a file and durably record the directory update. Missing files are
/// already in the requested state.
pub fn remove_file_durable(path: &Path) -> AtomicMutationResult<()> {
    #[cfg(unix)]
    {
        remove_file_durable_unix(path)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        unsupported_mutation("durable file removal")
    }
}

/// Recursively remove a directory in a trusted, operator-owned tree and record
/// its disappearance durably where the platform supports directory syncing.
///
/// This pathname traversal is not safe against a process concurrently
/// substituting entries. Callers must first establish ownership/exclusion.
pub fn remove_dir_all_durable(path: &Path) -> Result<()> {
    #[cfg(not(unix))]
    anyhow::bail!("trusted-tree durable removal is unavailable on this platform");

    #[cfg(unix)]
    match fs::remove_dir_all(path) {
        Ok(()) => {
            sync_parent_dir(path)?;
            Ok(())
        }
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

/// Flush every regular file and directory in a materialized tree.
///
/// Call this before a staged directory is renamed or exchanged into a live
/// namespace. Files are synced before their containing directories, so a
/// successful return establishes a durability barrier for both file contents
/// and the directory entries that make the tree reachable. Symlinks are not
/// followed; syncing their parent directory makes the link entry durable.
pub fn sync_tree_durable(root: &Path) -> Result<()> {
    #[cfg(not(unix))]
    anyhow::bail!("durable tree sync is unavailable on this platform");

    let metadata = fs::symlink_metadata(root)?;
    if metadata.file_type().is_symlink() {
        anyhow::bail!(
            "durable tree root must not be a symlink: {}",
            root.display()
        );
    }
    if metadata.is_file() {
        fs::File::open(root)?.sync_all()?;
        return Ok(());
    }
    if !metadata.is_dir() {
        anyhow::bail!(
            "durable tree root must be a file or directory: {}",
            root.display()
        );
    }
    sync_directory_tree(root)
}

fn sync_directory_tree(directory: &Path) -> Result<()> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            sync_directory_tree(&path)?;
        } else if file_type.is_file() {
            fs::File::open(&path)?.sync_all()?;
        } else if !file_type.is_symlink() {
            anyhow::bail!("unsupported entry in durable tree: {}", path.display());
        }
    }
    sync_directory_entry(directory)?;
    Ok(())
}

#[cfg(unix)]
fn sync_directory_entry(directory: &Path) -> std::io::Result<()> {
    fs::File::open(directory)?.sync_all()
}

#[cfg(not(unix))]
fn sync_directory_entry(_directory: &Path) -> std::io::Result<()> {
    Ok(())
}

/// Atomically exchange two existing sibling filesystem entries.
///
/// RyeOS uses this for live bundle generations: the installed path always
/// names either the complete old tree or the complete staged tree. Platforms
/// without an atomic exchange primitive are rejected rather than using a
/// remove-then-rename compatibility path.
#[cfg(target_os = "linux")]
pub fn atomic_exchange_paths(left: &Path, right: &Path) -> AtomicMutationResult<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    if left.parent() != right.parent() {
        return Err(AtomicMutationError::before(anyhow::anyhow!(
            "atomic exchange paths must share a parent directory"
        )));
    }
    let (parent, left_name) = open_parent_and_name(left)?;
    let right_name = right.file_name().ok_or_else(|| {
        AtomicMutationError::before(anyhow::anyhow!("exchange target has no file name"))
    })?;
    let right_c = CString::new(right_name.as_bytes()).map_err(AtomicMutationError::before)?;
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            std::os::fd::AsRawFd::as_raw_fd(&parent),
            left_name.as_ptr(),
            std::os::fd::AsRawFd::as_raw_fd(&parent),
            right_c.as_ptr(),
            libc::RENAME_EXCHANGE,
        )
    };
    if result != 0 {
        return Err(AtomicMutationError::before(std::io::Error::last_os_error()));
    }
    sync_open_parent(&parent).map_err(AtomicMutationError::durability)
}

#[cfg(not(target_os = "linux"))]
pub fn atomic_exchange_paths(_left: &Path, _right: &Path) -> AtomicMutationResult<()> {
    Err(AtomicMutationError::before(anyhow::anyhow!(
        "atomic filesystem exchange is unavailable on this platform"
    )))
}

/// Rename a staged entry into place and durably flush its parent directory.
pub fn rename_path_durable(source: &Path, target: &Path) -> AtomicMutationResult<()> {
    if source.parent() != target.parent() {
        return Err(AtomicMutationError::before(anyhow::anyhow!(
            "durable rename paths must share a parent directory"
        )));
    }
    #[cfg(unix)]
    {
        use std::ffi::CString;
        use std::os::fd::AsRawFd;
        use std::os::unix::ffi::OsStrExt;

        let (parent, source_name) = open_parent_and_name(source)?;
        let target_name = target.file_name().ok_or_else(|| {
            AtomicMutationError::before(anyhow::anyhow!("rename target has no file name"))
        })?;
        let target_name =
            CString::new(target_name.as_bytes()).map_err(AtomicMutationError::before)?;
        let renamed = unsafe {
            libc::renameat(
                parent.as_raw_fd(),
                source_name.as_ptr(),
                parent.as_raw_fd(),
                target_name.as_ptr(),
            )
        };
        if renamed != 0 {
            return Err(AtomicMutationError::before(std::io::Error::last_os_error()));
        }
        sync_open_parent(&parent).map_err(AtomicMutationError::durability)
    }
    #[cfg(not(unix))]
    {
        let _ = (source, target);
        unsupported_mutation("durable rename")
    }
}

/// Publish a staged sibling without replacing any existing destination, then
/// durably flush the shared parent directory.
pub fn rename_path_noreplace_durable(source: &Path, target: &Path) -> AtomicMutationResult<()> {
    if source.parent() != target.parent() {
        return Err(AtomicMutationError::before(anyhow::anyhow!(
            "durable no-replace rename paths must share a parent directory"
        )));
    }
    #[cfg(target_os = "linux")]
    {
        use std::ffi::CString;
        use std::os::fd::AsRawFd;
        use std::os::unix::ffi::OsStrExt;

        let (parent, source_name) = open_parent_and_name(source)?;
        let target_name = target.file_name().ok_or_else(|| {
            AtomicMutationError::before(anyhow::anyhow!("rename target has no file name"))
        })?;
        let target_name =
            CString::new(target_name.as_bytes()).map_err(AtomicMutationError::before)?;
        let renamed = unsafe {
            libc::renameat2(
                parent.as_raw_fd(),
                source_name.as_ptr(),
                parent.as_raw_fd(),
                target_name.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        };
        if renamed != 0 {
            return Err(AtomicMutationError::before(std::io::Error::last_os_error()));
        }
        return sync_open_parent(&parent).map_err(AtomicMutationError::durability);
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (source, target);
        Err(AtomicMutationError::before(anyhow::anyhow!(
            "durable no-replace rename is unavailable on this platform"
        )))
    }
}

/// Private atomic replacement relative to an already-open final parent.
///
/// Ancestor symlinks remain supported for app-root compatibility, but the
/// final parent itself must be a real directory. Holding its descriptor across
/// create, rename, and fsync prevents a concurrent parent swap from redirecting
/// secret material.
#[cfg(unix)]
fn atomic_write_unix(target: &Path, data: &[u8], mode: u32) -> AtomicMutationResult<()> {
    use std::ffi::CString;
    use std::os::fd::{AsRawFd, FromRawFd};

    let (parent_file, target_name) = open_parent_and_name(target)?;
    let file_name = target
        .file_name()
        .expect("open_parent_and_name validated name");
    let mut last_collision = None;

    for _ in 0..128 {
        let sequence = next_temp_sequence();
        let tmp_name = CString::new(format!(
            ".{}.tmp.{}.{sequence}",
            file_name.to_string_lossy(),
            std::process::id()
        ))
        .map_err(AtomicMutationError::before)?;
        let fd = unsafe {
            libc::openat(
                parent_file.as_raw_fd(),
                tmp_name.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                mode,
            )
        };
        if fd < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == ErrorKind::AlreadyExists {
                last_collision = Some(err);
                continue;
            }
            return Err(AtomicMutationError::before(err));
        }

        let mut tmp_file = unsafe { fs::File::from_raw_fd(fd) };
        let write_result = (|| -> std::io::Result<()> {
            tmp_file.write_all(data)?;
            tmp_file.sync_all()?;
            drop(tmp_file);
            Ok(())
        })();

        if let Err(err) = write_result {
            unsafe {
                libc::unlinkat(parent_file.as_raw_fd(), tmp_name.as_ptr(), 0);
            }
            return Err(AtomicMutationError::before(err));
        }
        let renamed = unsafe {
            libc::renameat(
                parent_file.as_raw_fd(),
                tmp_name.as_ptr(),
                parent_file.as_raw_fd(),
                target_name.as_ptr(),
            )
        };
        if renamed != 0 {
            let error = std::io::Error::last_os_error();
            unsafe {
                libc::unlinkat(parent_file.as_raw_fd(), tmp_name.as_ptr(), 0);
            }
            return Err(AtomicMutationError::before(error));
        }
        return sync_open_parent(&parent_file).map_err(AtomicMutationError::durability);
    }

    Err(AtomicMutationError::before(last_collision.unwrap_or_else(
        || std::io::Error::new(ErrorKind::AlreadyExists, "temp file collision"),
    )))
}

#[cfg(unix)]
fn open_parent_and_name(target: &Path) -> AtomicMutationResult<(fs::File, std::ffi::CString)> {
    use std::ffi::CString;
    use std::os::fd::FromRawFd;
    use std::os::unix::ffi::OsStrExt;

    let parent = target.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).map_err(AtomicMutationError::before)?;
    let parent_c =
        CString::new(parent.as_os_str().as_bytes()).map_err(AtomicMutationError::before)?;
    let parent_fd = unsafe {
        libc::open(
            parent_c.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    };
    if parent_fd < 0 {
        return Err(AtomicMutationError::before(std::io::Error::last_os_error()));
    }
    let parent_file = unsafe { fs::File::from_raw_fd(parent_fd) };
    let file_name = target.file_name().ok_or_else(|| {
        AtomicMutationError::before(anyhow::anyhow!("atomic target has no file name"))
    })?;
    let file_name = CString::new(file_name.as_bytes()).map_err(AtomicMutationError::before)?;
    Ok((parent_file, file_name))
}

#[cfg(unix)]
fn remove_file_durable_unix(path: &Path) -> AtomicMutationResult<()> {
    use std::os::fd::AsRawFd;

    let (parent, name) = open_parent_and_name(path)?;
    let removed = unsafe { libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), 0) };
    if removed != 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() == ErrorKind::NotFound {
            return Ok(());
        }
        return Err(AtomicMutationError::before(error));
    }
    sync_open_parent(&parent).map_err(AtomicMutationError::durability)
}

#[cfg(not(unix))]
fn unsupported_mutation(operation: &str) -> AtomicMutationResult<()> {
    Err(AtomicMutationError::before(anyhow::anyhow!(
        "{operation} is unavailable on this platform"
    )))
}

#[cfg(unix)]
fn sync_parent_dir(target: &Path) -> std::io::Result<()> {
    let parent = fs::File::open(target.parent().unwrap_or_else(|| Path::new(".")))?;
    sync_open_parent(&parent)
}

#[cfg(unix)]
fn sync_open_parent(parent: &fs::File) -> std::io::Result<()> {
    injected_parent_sync_result()?;
    parent.sync_all()
}

#[cfg(all(test, unix))]
thread_local! {
    static FAIL_NEXT_PARENT_SYNC: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(unix)]
fn injected_parent_sync_result() -> std::io::Result<()> {
    #[cfg(test)]
    if FAIL_NEXT_PARENT_SYNC.with(|fail| fail.replace(false)) {
        return Err(std::io::Error::other("injected parent sync failure"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn fail_next_parent_sync() {
        FAIL_NEXT_PARENT_SYNC.with(|fail| fail.set(true));
    }

    #[test]
    fn private_replace_writes_complete_value_and_leaves_no_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("secret.pem");
        atomic_write_private(&target, b"old").unwrap();
        atomic_write_private(&target, b"new-value").unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"new-value");
        let names: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names.len(), 1);
        assert_eq!(names[0], target.file_name().unwrap());
    }

    #[test]
    fn write_error_before_rename_reports_not_committed() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("missing-parent").join("value");
        fs::create_dir(target.parent().unwrap()).unwrap();
        fs::remove_dir(target.parent().unwrap()).unwrap();
        fs::write(dir.path().join("missing-parent"), b"not a directory").unwrap();

        let error = atomic_write(&target, b"new").unwrap_err();

        assert!(matches!(&error, AtomicMutationError::BeforeCommit(_)));
        assert!(!error.namespace_committed());
        assert!(!target.exists());
    }

    #[cfg(unix)]
    #[test]
    fn write_parent_sync_failure_reports_committed_but_uncertain() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("value");
        fs::write(&target, b"old").unwrap();
        fail_next_parent_sync();

        let error = atomic_write(&target, b"new").unwrap_err();

        assert!(matches!(
            &error,
            AtomicMutationError::DurabilityUncertain(_)
        ));
        assert!(error.namespace_committed());
        assert_eq!(fs::read(target).unwrap(), b"new");
    }

    #[cfg(unix)]
    #[test]
    fn rename_parent_sync_failure_reports_committed_but_uncertain() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source");
        let target = dir.path().join("target");
        fs::write(&source, b"value").unwrap();
        fail_next_parent_sync();

        let error = rename_path_durable(&source, &target).unwrap_err();

        assert!(matches!(error, AtomicMutationError::DurabilityUncertain(_)));
        assert!(!source.exists());
        assert_eq!(fs::read(target).unwrap(), b"value");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn exchange_parent_sync_failure_reports_committed_but_uncertain() {
        let dir = tempfile::tempdir().unwrap();
        let left = dir.path().join("left");
        let right = dir.path().join("right");
        fs::create_dir(&left).unwrap();
        fs::create_dir(&right).unwrap();
        fs::write(left.join("value"), b"left").unwrap();
        fs::write(right.join("value"), b"right").unwrap();
        fail_next_parent_sync();

        let error = atomic_exchange_paths(&left, &right).unwrap_err();

        assert!(matches!(error, AtomicMutationError::DurabilityUncertain(_)));
        assert_eq!(fs::read(left.join("value")).unwrap(), b"right");
        assert_eq!(fs::read(right.join("value")).unwrap(), b"left");
    }

    #[cfg(unix)]
    #[test]
    fn private_replace_creates_mode_0600() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("secret.pem");
        atomic_write_private(&target, b"secret").unwrap();
        assert_eq!(
            fs::metadata(target).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[cfg(unix)]
    #[test]
    fn private_replace_rejects_symlink_as_final_parent() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let real_parent = dir.path().join("real");
        fs::create_dir(&real_parent).unwrap();
        let linked_parent = dir.path().join("linked");
        symlink(&real_parent, &linked_parent).unwrap();
        let result = atomic_write_private(&linked_parent.join("secret.pem"), b"secret");
        assert!(result.is_err());
        assert!(!real_parent.join("secret.pem").exists());
    }

    #[cfg(unix)]
    #[test]
    fn general_replace_rejects_symlink_as_final_parent() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let real_parent = dir.path().join("real");
        fs::create_dir(&real_parent).unwrap();
        let linked_parent = dir.path().join("linked");
        symlink(&real_parent, &linked_parent).unwrap();

        let error = atomic_write(&linked_parent.join("value"), b"value").unwrap_err();

        assert!(matches!(error, AtomicMutationError::BeforeCommit(_)));
        assert!(!real_parent.join("value").exists());
    }

    #[cfg(unix)]
    #[test]
    fn mode_aware_write_sets_mode_before_publication() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("executable");
        atomic_write_with_mode(&target, b"payload", 0o751).unwrap();

        assert_eq!(
            fs::metadata(target).unwrap().permissions().mode() & 0o777,
            0o751
        );
    }

    #[cfg(unix)]
    #[test]
    fn durable_remove_rejects_symlink_as_final_parent() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let real_parent = dir.path().join("real");
        fs::create_dir(&real_parent).unwrap();
        let target = real_parent.join("keep");
        fs::write(&target, b"value").unwrap();
        let linked_parent = dir.path().join("linked");
        symlink(&real_parent, &linked_parent).unwrap();

        let error = remove_file_durable(&linked_parent.join("keep")).unwrap_err();

        assert!(matches!(error, AtomicMutationError::BeforeCommit(_)));
        assert!(target.exists());
    }
}
