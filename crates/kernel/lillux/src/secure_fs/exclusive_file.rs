//! Exclusive creation with actual descriptor custody on post-open refusal.
use super::PinnedDirectory;
use anyhow::{Context, Result};
use std::{
    ffi::{OsStr, OsString},
    fs::File,
    sync::Arc,
};

/// Retains the original namespace and every acquired file. An absent file is
/// not proof that a refused open had no filesystem effect. No removal or
/// replacement is attempted on Drop or inferred from the retained coordinate.
pub struct ExclusiveFileCreationFailure {
    error: anyhow::Error,
    parent: Arc<PinnedDirectory>,
    name: OsString,
    file: Option<File>,
}
impl ExclusiveFileCreationFailure {
    pub fn original_error(&self) -> &anyhow::Error {
        &self.error
    }
    pub fn original_parent(&self) -> &PinnedDirectory {
        &self.parent
    }
    pub fn child_name(&self) -> &OsStr {
        &self.name
    }
    pub fn acquired_file(&self) -> Option<&File> {
        self.file.as_ref()
    }
}
impl std::fmt::Debug for ExclusiveFileCreationFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExclusiveFileCreationFailure")
            .field("error", &self.error)
            .field("file_retained", &self.file.is_some())
            .finish_non_exhaustive()
    }
}
impl std::fmt::Display for ExclusiveFileCreationFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error.fmt(f)
    }
}
impl std::error::Error for ExclusiveFileCreationFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.error.as_ref())
    }
}

impl PinnedDirectory {
    /// Create a new writable regular child, never replacing an existing entry.
    /// Post-open metadata/binding failures retain the actual file descriptor.
    /// This performs no parent sync and does not grant mutation authority.
    pub fn create_regular_exclusive(
        self: &Arc<Self>,
        name: &OsStr,
        mode: u32,
    ) -> std::result::Result<File, ExclusiveFileCreationFailure> {
        let mut retained = ExclusiveFileCreationFailure {
            error: anyhow::anyhow!("exclusive regular-file creation pending"),
            parent: Arc::clone(self),
            name: name.to_os_string(),
            file: None,
        };
        let result = (|| -> Result<()> {
            super::validate_child_name(name)?;
            self.ensure_path_binding()?;
            #[cfg(not(unix))]
            anyhow::bail!("exclusive regular-file creation unavailable on this platform");
            #[cfg(unix)]
            {
                use std::os::{
                    fd::{AsRawFd, FromRawFd},
                    unix::ffi::OsStrExt,
                };
                let name_c = std::ffi::CString::new(name.as_bytes())?;
                let descriptor = unsafe {
                    libc::openat(
                        self.directory.as_raw_fd(),
                        name_c.as_ptr(),
                        libc::O_RDWR
                            | libc::O_CREAT
                            | libc::O_EXCL
                            | libc::O_NOFOLLOW
                            | libc::O_CLOEXEC
                            | libc::O_NONBLOCK,
                        mode as libc::mode_t,
                    )
                };
                if descriptor < 0 {
                    return Err(std::io::Error::last_os_error())
                        .context("exclusive regular-file open refused");
                }
                // Transfer the real FD before any fallible observation.
                retained.file = Some(unsafe { File::from_raw_fd(descriptor) });
                let file = retained
                    .file
                    .as_ref()
                    .expect("acquired exclusive descriptor");
                anyhow::ensure!(
                    file.metadata()?.file_type().is_file(),
                    "exclusive child is not a regular file"
                );
                self.ensure_path_binding()?;
            }
            Ok(())
        })();
        match result {
            Ok(()) => Ok(retained
                .file
                .take()
                .expect("validated exclusive descriptor")),
            Err(error) => {
                retained.error = error;
                Err(retained)
            }
        }
    }
}
