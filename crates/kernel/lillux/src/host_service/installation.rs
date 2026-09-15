//! Administrator-owned package installation transactions.
//!
//! The caller selects a signed/package-specific script and its environment;
//! Lillux owns exact file pinning, namespace locking, descriptor inheritance,
//! and exec. No application may reconstruct this with ambient paths or a
//! process-local mutex.

use std::ffi::OsString;
use std::path::{Component, Path};
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::{
    PinnedDirectory, PinnedRegularFile, configure_inherited_descriptor_authorities,
    require_administrator,
};

fn pin_administrator_regular(path: &Path) -> Result<PinnedRegularFile> {
    let parent = path
        .parent()
        .context("host installer executable has no parent")?;
    let directory = PinnedDirectory::open_owned_hierarchy(parent, 0)?
        .context("host installer executable directory is absent")?;
    let file = directory
        .open_pinned_regular(
            path.file_name()
                .context("host installer executable has no filename")?,
            false,
        )?
        .context("host installer executable is absent")?;
    file.require_owner(0)?;
    file.require_executable()?;
    Ok(file)
}

fn admit_operator_script(path: &Path, expected_digest: &str) -> Result<PinnedRegularFile> {
    if !crate::valid_hash(expected_digest) {
        bail!("installer script requires an exact SHA-256 digest");
    }
    let parent = PinnedDirectory::open(path.parent().context("installer script has no parent")?)?
        .context("installer script directory is absent")?;
    let script = parent
        .open_pinned_regular(
            path.file_name()
                .context("installer script has no filename")?,
            false,
        )?
        .context("installer script is absent")?;
    let observation = script.observation()?;
    let digest = script.digest_stable_exact(&observation)?;
    if digest != expected_digest {
        bail!("installer script changed after its explicit admission");
    }
    // This is the explicit developer-root boundary. A checkout script is not
    // a packaged workload and may be owned by the operator; its exact digest
    // records and verifies the bytes observed at root admission. Replacing this boundary with a
    // signed development Tool requires migrating the complete invoked script
    // closure, not sealing this one file while it still sources companions.
    Ok(script)
}

fn package_namespace(path: &Path) -> Result<PinnedDirectory> {
    if !path.is_absolute()
        || path.parent().is_none()
        || path
            .components()
            .any(|component| !matches!(component, Component::RootDir | Component::Normal(_)))
    {
        bail!("host package namespace must be a normalized non-root absolute path");
    }
    let parent = PinnedDirectory::open_owned_hierarchy(path.parent().expect("checked parent"), 0)?
        .context("host package namespace parent is absent")?;
    let namespace =
        parent.open_or_create_child(path.file_name().expect("checked parent"), 0o755)?;
    namespace.require_owner(0)?;
    Ok(namespace)
}

/// Retain the exact package-namespace lock across exec into one explicitly
/// admitted operator script. The descriptor is the authority;
/// `transaction_fd_env` only carries its inherited coordinate to the
/// re-exec'd script.
pub fn exec_install_transaction(
    package_root: &Path,
    interpreter: &Path,
    script: &Path,
    script_digest: &str,
    arguments: &[OsString],
    environment: &[(OsString, OsString)],
    transaction_fd_env: &str,
) -> Result<std::convert::Infallible> {
    require_administrator()?;
    let namespace = package_namespace(package_root)?;
    let interpreter = pin_administrator_regular(interpreter)?.inherited_descriptor_authority()?;
    let script = admit_operator_script(script, script_digest)?;
    let mut command = Command::new(interpreter.path());
    command
        .arg(script.path())
        .args(arguments)
        .envs(environment.iter().cloned());
    configure_inherited_descriptor_authorities(&mut command, &[interpreter])
        .map_err(anyhow::Error::msg)?;
    namespace
        .lock_exclusive()?
        .exec_command_with_lock(&mut command, transaction_fd_env)
}

/// Validate the inherited installation lock after script exec before any
/// package mutation. This deliberately does not manufacture a new lock.
pub fn validate_install_transaction(package_root: &Path, descriptor: u32) -> Result<()> {
    require_administrator()?;
    let namespace = PinnedDirectory::open_owned_hierarchy(package_root, 0)?
        .context("host package namespace is absent")?;
    namespace.require_inherited_exclusive_lock(descriptor)?;
    Ok(())
}

/// Request the host's administrator boundary for one exact root-owned program.
/// Applications never name an elevation utility or a platform-specific prompt.
/// This is a one-shot maintenance boundary, not a worker capability or daemon
/// broker; the selected program is pinned before the elevation process starts.
pub fn run_as_administrator(program: &Path, arguments: &[OsString]) -> Result<()> {
    // Elevation utilities commonly close inherited descriptors before exec.
    // Validate the target as an exact root-owned image here, then pass its
    // administrator-owned absolute pathname across that boundary. Retaining an
    // FD and spelling `/proc/self/fd/N` would be a false authority claim.
    let program = pin_administrator_regular(program)?;
    #[cfg(unix)]
    {
        let elevating = pin_administrator_regular(Path::new("/usr/bin/sudo"))?
            .inherited_descriptor_authority()?;
        let mut command = Command::new(elevating.path());
        command.env_clear().arg(program.path()).args(arguments);
        configure_inherited_descriptor_authorities(&mut command, &[elevating])
            .map_err(anyhow::Error::msg)?;
        let status = command
            .status()
            .context("request host administrator authority")?;
        if !status.success() {
            bail!("host administrator command failed: {status}");
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = (program, arguments);
        bail!("host administrator elevation is unavailable on this OS")
    }
}
