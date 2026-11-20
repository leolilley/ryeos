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

/// Maximum symlink hops accepted while locating the elevation utility.
#[cfg(unix)]
const ELEVATION_MAX_LINK_HOPS: usize = 8;
/// Longest resolved elevation path accepted (Linux PATH_MAX).
#[cfg(unix)]
const ELEVATION_MAX_PATH_BYTES: usize = 4096;

/// Lexically normalize an absolute path. This is exact here because every hop's
/// directory is proven owner-controlled and opened without following links, so
/// no component of `parent` can itself be a link that `..` would cross.
#[cfg(unix)]
fn normalize_absolute(path: &Path) -> Result<std::path::PathBuf> {
    let mut out = std::path::PathBuf::from("/");
    if !path.is_absolute() {
        bail!("elevation path is not absolute");
    }
    for component in path.components() {
        match component {
            Component::RootDir | Component::CurDir => {}
            Component::Normal(name) => out.push(name),
            Component::ParentDir => {
                if !out.pop() {
                    bail!("elevation link target escapes the filesystem root");
                }
            }
            Component::Prefix(_) => bail!("elevation path has a platform prefix"),
        }
    }
    Ok(out)
}

/// Resolve `path` one link hop at a time. Each hop's directory must satisfy
/// `check_directory`, and each link must be owned by `owner`. Absolute and
/// relative targets are followed; loops, chains longer than
/// `ELEVATION_MAX_LINK_HOPS`, over-long paths, and dangling links are refused.
/// Returns the final non-link path, which the caller still pins exactly.
///
/// Symlink permission bits carry no authority on Linux (always 0777), so link
/// writability is enforced through the owner-controlled directory that holds
/// the link, which is what decides who may replace it.
#[cfg(unix)]
fn resolve_elevation_links(
    path: &Path,
    owner: u32,
    check_directory: &dyn Fn(&Path) -> Result<()>,
) -> Result<std::path::PathBuf> {
    use std::os::unix::ffi::OsStrExt as _;
    use std::os::unix::fs::MetadataExt as _;

    let mut current = normalize_absolute(path)?;
    let mut seen = std::collections::HashSet::new();
    let mut hops = 0usize;
    loop {
        if current.as_os_str().as_bytes().len() > ELEVATION_MAX_PATH_BYTES {
            bail!("elevation path exceeds the length limit");
        }
        if !seen.insert(current.clone()) {
            bail!("elevation link chain loops at {}", current.display());
        }
        let parent = current
            .parent()
            .context("elevation path has no parent directory")?;
        check_directory(parent)?;
        let metadata = std::fs::symlink_metadata(&current)
            .with_context(|| format!("inspect elevation path {}", current.display()))?;
        if !metadata.file_type().is_symlink() {
            return Ok(current);
        }
        hops += 1;
        if hops > ELEVATION_MAX_LINK_HOPS {
            bail!("elevation link chain exceeds {ELEVATION_MAX_LINK_HOPS} hops");
        }
        if metadata.uid() != owner {
            bail!(
                "elevation link {} is not owned by the administrator",
                current.display()
            );
        }
        let target = std::fs::read_link(&current)
            .with_context(|| format!("read elevation link {}", current.display()))?;
        current = normalize_absolute(&parent.join(target))?;
    }
}

#[cfg(unix)]
fn pin_administrator_elevator(path: &Path) -> Result<PinnedRegularFile> {
    let resolved = resolve_elevation_links(path, 0, &|directory| {
        PinnedDirectory::open_owned_hierarchy(directory, 0)?
            .map(|_| ())
            .context("elevation directory is absent")
    })?;
    pin_administrator_regular(&resolved)
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
        let elevating = pin_administrator_elevator(Path::new("/usr/bin/sudo"))?
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

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt as _, symlink};

    fn uid() -> u32 {
        use std::os::unix::fs::MetadataExt as _;
        std::fs::metadata(".").unwrap().uid()
    }

    /// Mirrors the production directory proof with the owner as a parameter.
    fn dir_ok(owner: u32) -> impl Fn(&Path) -> Result<()> {
        move |path| {
            PinnedDirectory::open(path)?
                .context("absent")?
                .require_owner(owner)
        }
    }

    fn tree() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        dir
    }

    fn real(root: &Path) -> std::path::PathBuf {
        let target = root.join("real");
        std::fs::write(&target, b"x").unwrap();
        target
    }

    #[test]
    fn plain_regular_file_resolves_to_itself() {
        let dir = tree();
        let target = real(dir.path());
        let resolved = resolve_elevation_links(&target, uid(), &dir_ok(uid())).unwrap();
        assert_eq!(resolved, target);
    }

    #[test]
    fn absolute_and_relative_links_resolve() {
        let dir = tree();
        let target = real(dir.path());
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::set_permissions(
            dir.path().join("sub"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        symlink(&target, dir.path().join("abs")).unwrap();
        symlink("../real", dir.path().join("sub/rel")).unwrap();
        symlink("abs", dir.path().join("chain")).unwrap();
        for link in ["abs", "sub/rel", "chain"] {
            let resolved =
                resolve_elevation_links(&dir.path().join(link), uid(), &dir_ok(uid())).unwrap();
            assert_eq!(resolved, target, "{link}");
        }
    }

    #[test]
    fn link_with_wrong_owner_is_refused() {
        let dir = tree();
        let target = real(dir.path());
        symlink(&target, dir.path().join("l")).unwrap();
        let error =
            resolve_elevation_links(&dir.path().join("l"), uid().wrapping_add(1), &|_| Ok(()))
                .unwrap_err();
        assert!(error.to_string().contains("not owned"), "{error}");
    }

    #[test]
    fn writable_link_directory_is_refused() {
        let dir = tree();
        let target = real(dir.path());
        let open = dir.path().join("open");
        std::fs::create_dir(&open).unwrap();
        std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o775)).unwrap();
        symlink(&target, open.join("l")).unwrap();
        assert!(resolve_elevation_links(&open.join("l"), uid(), &dir_ok(uid())).is_err());
    }

    #[test]
    fn loop_is_refused() {
        let dir = tree();
        symlink("b", dir.path().join("a")).unwrap();
        symlink("a", dir.path().join("b")).unwrap();
        let error =
            resolve_elevation_links(&dir.path().join("a"), uid(), &dir_ok(uid())).unwrap_err();
        assert!(error.to_string().contains("loops"), "{error}");
    }

    #[test]
    fn chain_too_long_is_refused() {
        let dir = tree();
        let target = real(dir.path());
        let mut previous = target;
        for index in 0..=ELEVATION_MAX_LINK_HOPS {
            let link = dir.path().join(format!("l{index}"));
            symlink(&previous, &link).unwrap();
            previous = link;
        }
        let error = resolve_elevation_links(&previous, uid(), &dir_ok(uid())).unwrap_err();
        assert!(error.to_string().contains("hops"), "{error}");
    }

    #[test]
    fn dangling_link_is_refused() {
        let dir = tree();
        symlink("missing", dir.path().join("l")).unwrap();
        assert!(resolve_elevation_links(&dir.path().join("l"), uid(), &dir_ok(uid())).is_err());
    }

    #[test]
    fn link_escaping_root_is_refused() {
        let dir = tree();
        symlink("../../../../../../../../../../..", dir.path().join("l")).unwrap();
        // Normalizes to "/" at worst; the directory check or parent lookup refuses.
        assert!(resolve_elevation_links(&dir.path().join("l"), uid(), &dir_ok(uid())).is_err());
    }

    /// The host's own elevation utility must pin through whatever links it
    /// sits behind. On a host whose `/usr/bin/sudo` is a link (sudo-rs) this is
    /// the real chain; elsewhere it is the plain regular file. A host without
    /// sudo has nothing to prove.
    #[test]
    fn the_hosts_own_sudo_pins_through_its_links() {
        let sudo = Path::new("/usr/bin/sudo");
        if std::fs::symlink_metadata(sudo).is_err() {
            return;
        }
        pin_administrator_elevator(sudo).expect("the host's own sudo pins");
    }
}
