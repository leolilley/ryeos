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

/// Copy the dynamic loader and shared libraries that `binaries` need into a
/// runtime tree rooted at `root`, so the binaries run from that tree alone.
///
/// The loader lands at its own interpreter path (the one recorded in the ELF
/// program headers) beneath `root`; each needed shared object lands in
/// `root/usr/lib` under its soname. The host's spelling of those locations is
/// discovered here, never assumed: the interpreter is read from the binary's
/// `PT_INTERP` header and the libraries are resolved by one bounded
/// `<loader> --list <binary>` call, which is the resolution `ldd` performs.
/// Every file is made owner-private and executable.
#[cfg(all(unix, target_os = "linux"))]
pub fn install_native_runtime_libraries(
    root: &std::path::Path,
    binaries: &[&std::path::Path],
) -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::{Path, PathBuf};

    fn copy_private(source: &Path, destination: &Path) -> Result<()> {
        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create {}", parent.display()))?;
        }
        std::fs::copy(source, destination)
            .with_context(|| format!("copy {} to {}", source.display(), destination.display()))?;
        std::fs::set_permissions(destination, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("restrict {}", destination.display()))
    }

    fn elf_interpreter(binary: &Path) -> Result<PathBuf> {
        use std::io::{Read as _, Seek as _, SeekFrom};
        const PT_INTERP: u32 = 3;
        const MAX_HEADER_TABLE: u64 = 64 * 1024;
        const MAX_INTERPRETER: u64 = 4096;
        let mut file =
            std::fs::File::open(binary).with_context(|| format!("open {}", binary.display()))?;
        let mut header = [0u8; 64];
        file.read_exact(&mut header)
            .with_context(|| format!("read ELF header of {}", binary.display()))?;
        anyhow::ensure!(
            header[..4] == *b"\x7fELF" && header[4] == 2 && header[5] == 1,
            "{} is not a little-endian ELF64 binary",
            binary.display()
        );
        let u16_at = |at: usize| u64::from(u16::from_le_bytes([header[at], header[at + 1]]));
        let u64_at = |at: usize| {
            let mut bytes = [0u8; 8];
            bytes.copy_from_slice(&header[at..at + 8]);
            u64::from_le_bytes(bytes)
        };
        let (table, entry, count) = (u64_at(0x20), u16_at(0x36), u16_at(0x38));
        anyhow::ensure!(
            entry >= 56 && entry * count <= MAX_HEADER_TABLE,
            "{} has an implausible program header table",
            binary.display()
        );
        for index in 0..count {
            let mut phdr = vec![0u8; entry as usize];
            file.seek(SeekFrom::Start(table + index * entry))?;
            file.read_exact(&mut phdr)?;
            if u32::from_le_bytes([phdr[0], phdr[1], phdr[2], phdr[3]]) != PT_INTERP {
                continue;
            }
            let mut field = [0u8; 8];
            field.copy_from_slice(&phdr[8..16]);
            let offset = u64::from_le_bytes(field);
            field.copy_from_slice(&phdr[32..40]);
            let size = u64::from_le_bytes(field);
            anyhow::ensure!(
                size > 1 && size <= MAX_INTERPRETER,
                "{} has an implausible interpreter size",
                binary.display()
            );
            let mut name = vec![0u8; size as usize];
            file.seek(SeekFrom::Start(offset))?;
            file.read_exact(&mut name)?;
            name.pop();
            return Ok(PathBuf::from(
                String::from_utf8(name).context("interpreter path is not UTF-8")?,
            ));
        }
        anyhow::bail!(
            "{} has no interpreter (statically linked?)",
            binary.display()
        )
    }

    let mut loader: Option<PathBuf> = None;
    let mut libraries: std::collections::BTreeMap<String, PathBuf> = Default::default();
    for binary in binaries {
        let interpreter = elf_interpreter(binary)?;
        anyhow::ensure!(
            loader
                .as_ref()
                .is_none_or(|existing| *existing == interpreter),
            "runtime binaries disagree about their dynamic loader"
        );
        loader = Some(interpreter.clone());
        let listing = std::process::Command::new(&interpreter)
            .arg("--list")
            .arg(binary)
            .env_clear()
            .output()
            .with_context(|| format!("run {} --list", interpreter.display()))?;
        anyhow::ensure!(
            listing.status.success(),
            "{} --list {} failed",
            interpreter.display(),
            binary.display()
        );
        for line in String::from_utf8_lossy(&listing.stdout).lines() {
            let Some((name, resolved)) = line.trim().split_once(" => ") else {
                continue;
            };
            let Some(resolved) = resolved.split(" (").next().map(str::trim) else {
                continue;
            };
            if resolved.starts_with('/') {
                libraries.insert(name.trim().to_owned(), PathBuf::from(resolved));
            }
        }
    }
    let loader = loader.context("no runtime binaries were named")?;
    let relative_loader = loader
        .strip_prefix("/")
        .with_context(|| format!("loader path {} is not absolute", loader.display()))?;
    copy_private(&loader, &root.join(relative_loader))?;
    for (name, source) in &libraries {
        anyhow::ensure!(
            !name.contains('/') && name != ".." && name != ".",
            "shared object name {name:?} is not a plain file name"
        );
        copy_private(source, &root.join("usr/lib").join(name))?;
    }
    Ok(())
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
