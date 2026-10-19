//! Platform facts observed at the host-mechanism boundary.
//!
//! Higher layers compare these opaque, portable labels but never select or
//! reproduce the operating-system mechanism used to obtain them.

use anyhow::{Context as _, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlatformTarget {
    pub os: &'static str,
    pub arch: &'static str,
}

pub fn current_target() -> PlatformTarget {
    PlatformTarget {
        os: std::env::consts::OS,
        arch: std::env::consts::ARCH,
    }
}

/// Exact bundle binary target implemented by this Lillux build.
///
/// Bundle composition is allowed to compare this opaque label. It must not
/// reproduce Rust target/OS/ABI selection above the Lillux boundary.
#[cfg(all(target_arch = "x86_64", target_os = "linux", target_env = "gnu"))]
pub fn current_binary_target() -> Result<&'static str> {
    Ok("x86_64-unknown-linux-gnu")
}

#[cfg(all(target_arch = "aarch64", target_os = "linux", target_env = "gnu"))]
pub fn current_binary_target() -> Result<&'static str> {
    Ok("aarch64-unknown-linux-gnu")
}

#[cfg(not(any(
    all(target_arch = "x86_64", target_os = "linux", target_env = "gnu"),
    all(target_arch = "aarch64", target_os = "linux", target_env = "gnu")
)))]
pub fn current_binary_target() -> Result<&'static str> {
    anyhow::bail!("this Lillux build has no supported native bundle binary target")
}

/// Open the host's temporary-directory root as exact filesystem authority.
///
/// The host spelling and its platform meaning remain in Lillux. Callers may
/// create bounded private descendants only through the returned pinned root.
#[cfg(unix)]
pub fn host_temporary_root() -> Result<crate::PinnedDirectory> {
    crate::PinnedDirectory::open(std::path::Path::new("/tmp"))?
        .context("host temporary-directory root is absent")
}

#[cfg(not(unix))]
pub fn host_temporary_root() -> Result<crate::PinnedDirectory> {
    anyhow::bail!("this platform has no qualified host temporary-directory root")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_binary_target_is_canonical_when_supported() {
        if let Ok(target) = current_binary_target() {
            assert!(!target.is_empty());
            assert!(!target.chars().any(char::is_whitespace));
        }
    }
}

/// Whether an I/O failure reflects a momentary shortage or interruption of
/// the host's resources rather than anything about the file or its content:
/// out of descriptors or memory, an interrupted or would-block call, or a
/// device I/O error. A caller may refuse the one operation and try again
/// later; such a failure is never evidence about durable state.
pub fn io_error_is_transient_resource(error: &std::io::Error) -> bool {
    use std::io::ErrorKind;
    if matches!(
        error.kind(),
        ErrorKind::Interrupted
            | ErrorKind::WouldBlock
            | ErrorKind::OutOfMemory
            | ErrorKind::TimedOut
    ) {
        return true;
    }
    #[cfg(unix)]
    {
        matches!(
            error.raw_os_error(),
            Some(
                libc::EMFILE
                    | libc::ENFILE
                    | libc::ENOMEM
                    | libc::ENOBUFS
                    | libc::EIO
                    | libc::EINTR
                    | libc::EAGAIN
            )
        )
    }
    #[cfg(not(unix))]
    {
        false
    }
}

#[cfg(all(test, unix))]
mod transient_resource_tests {
    #[test]
    fn descriptor_memory_io_and_interrupt_failures_are_transient() {
        for errno in [
            libc::EMFILE,
            libc::ENFILE,
            libc::ENOMEM,
            libc::EIO,
            libc::EINTR,
            libc::EAGAIN,
        ] {
            assert!(
                super::io_error_is_transient_resource(&std::io::Error::from_raw_os_error(errno)),
                "{errno}"
            );
        }
        for errno in [libc::ENOENT, libc::EACCES, libc::EEXIST, libc::ENOTDIR] {
            assert!(
                !super::io_error_is_transient_resource(&std::io::Error::from_raw_os_error(errno)),
                "{errno}"
            );
        }
        assert!(!super::io_error_is_transient_resource(
            &std::io::Error::other("tampered")
        ));
    }
}
