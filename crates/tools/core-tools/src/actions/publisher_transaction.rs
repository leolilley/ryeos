//! Atomic publisher-generation staging.
//!
//! Publisher operations author several mutually-dependent files. Updating
//! those files in the live bundle one at a time exposes an incomplete
//! generation when a later phase fails. This module copies the complete
//! bundle into a sibling staging directory, lets the caller finish and flush
//! every update there, then atomically exchanges the staged tree with the live
//! tree. The live path therefore always names one complete generation.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PublisherExchangeRecovery {
    live_device: u64,
    live_inode: u64,
    staging_device: u64,
    staging_inode: u64,
}

/// Run `author` against a private copy of `bundle_root`, then atomically make
/// the completed copy live.
///
/// The old generation is removed only after the exchange. Cleanup failure is
/// non-fatal because the complete new generation is already committed; a
/// warning preserves the evidence needed for operator cleanup.
pub(super) fn with_staged_bundle_generation<T>(
    bundle_root: &Path,
    author: impl FnOnce(&Path) -> Result<T>,
) -> Result<T> {
    require_real_directory(bundle_root, "publisher bundle root")?;
    let bundle_root = fs::canonicalize(bundle_root).with_context(|| {
        format!(
            "canonicalize publisher bundle root {}",
            bundle_root.display()
        )
    })?;
    let parent = bundle_root
        .parent()
        .ok_or_else(|| anyhow!("publisher bundle root has no parent"))?;
    require_real_directory(parent, "publisher bundle parent")?;
    let bundle_name = bundle_root
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow!("publisher bundle root has no UTF-8 directory name"))?;

    // Serialize the read-copy-author-exchange sequence. The persistent flock
    // anchor lives in an explicit sibling namespace: it must remain outside
    // the bundle directory that is atomically exchanged, but it is publisher
    // coordination rather than bundle content or CAS state.
    let lock_target = parent
        .join(".publisher-locks")
        .join(format!("{bundle_name}.publish"));
    let _lock = lillux::ExclusiveFileLock::acquire(&lock_target)
        .with_context(|| format!("lock publisher bundle {}", bundle_root.display()))?;

    let staging = create_staging_directory(parent, &bundle_root)?;
    let recovery_marker = publisher_recovery_marker(parent, bundle_name);
    let mut cleanup = StagingCleanup::new(staging.clone());

    copy_tree_contents(&bundle_root, &staging, Path::new("")).with_context(|| {
        format!(
            "stage publisher generation {} -> {}",
            bundle_root.display(),
            staging.display()
        )
    })?;

    let result = author(&staging)?;
    ensure_no_floor_excluded_content(&staging, Path::new(""))?;
    lillux::sync_tree_durable(&staging)
        .with_context(|| format!("flush staged publisher generation {}", staging.display()))?;
    let recovery = PublisherExchangeRecovery::capture(&bundle_root, &staging)?;
    lillux::atomic_write_private(
        &recovery_marker,
        &serde_json::to_vec(&recovery).context("serialize publisher exchange recovery marker")?,
    )
    .context("publish publisher exchange recovery marker")?;

    if let Err(error) = lillux::atomic_exchange_paths(&bundle_root, &staging) {
        if !error.namespace_committed() {
            let _ = lillux::remove_file_durable(&recovery_marker);
            return Err(error).with_context(|| {
                format!(
                    "atomically publish staged generation {} -> {}",
                    staging.display(),
                    bundle_root.display()
                )
            });
        }
        // The namespace exchange happened; only its durability barrier failed.
        // Returning an ordinary failure here would invite a caller to retry a
        // publication that is already visible.
        tracing::warn!(
            path = %bundle_root.display(),
            error = %error,
            "publisher generation committed but parent-directory durability is uncertain"
        );
    }

    if let Err(error) = restore_floor_excluded_content(&staging, &bundle_root, Path::new("")) {
        tracing::warn!(
            old_generation = %staging.display(),
            live_generation = %bundle_root.display(),
            error = %error,
            "publisher generation committed but local excluded content remains in the recoverable old generation"
        );
        cleanup.disarm();
        return Ok(result);
    }
    if let Err(error) = lillux::remove_file_durable(&recovery_marker) {
        tracing::warn!(
            path = %recovery_marker.display(),
            error = %error,
            "publisher generation committed but recovery-marker cleanup failed"
        );
    }
    if let Err(error) = lillux::remove_dir_all_durable(&staging) {
        tracing::warn!(
            path = %staging.display(),
            error = %error,
            "publisher generation committed but previous-generation cleanup failed"
        );
    }
    cleanup.disarm();
    Ok(result)
}

/// Redirect roots that name content inside `live_root` to the private staged
/// generation. Publisher validation must never mix files from the live and
/// staged generations, including when a bundle provides its own registries.
pub(super) fn roots_for_staged_generation(
    live_root: &Path,
    staging: &Path,
    roots: &[PathBuf],
) -> Vec<PathBuf> {
    let canonical_live = fs::canonicalize(live_root).ok();
    roots
        .iter()
        .map(|root| {
            let relative = canonical_live.as_ref().and_then(|live| {
                fs::canonicalize(root)
                    .ok()
                    .and_then(|canonical| canonical.strip_prefix(live).ok().map(Path::to_path_buf))
            });
            match relative {
                Some(relative) => staging.join(relative),
                None if root == live_root => staging.to_path_buf(),
                None => root.clone(),
            }
        })
        .collect()
}

fn create_staging_directory(parent: &Path, bundle_root: &Path) -> Result<PathBuf> {
    let name = bundle_root
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow!("publisher bundle root has no UTF-8 directory name"))?;

    let staging = parent.join(format!(".{name}.publish-staging"));
    let recovery_marker = publisher_recovery_marker(parent, name);
    match fs::symlink_metadata(&staging) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.file_type().is_dir() {
                bail!(
                    "stale publisher staging {} is not a real directory",
                    staging.display()
                );
            }
            recover_stale_staging(bundle_root, &staging, &recovery_marker)?;
            lillux::remove_dir_all_durable(&staging)
                .with_context(|| format!("remove stale publisher staging {}", staging.display()))?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            retire_orphan_recovery_marker(&recovery_marker)?;
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("inspect publisher staging {}", staging.display()));
        }
    }
    fs::create_dir(&staging)
        .with_context(|| format!("create publisher staging {}", staging.display()))?;
    Ok(staging)
}

fn publisher_recovery_marker(parent: &Path, bundle_name: &str) -> PathBuf {
    parent
        .join(".publisher-locks")
        .join(format!("{bundle_name}.exchange-recovery.json"))
}

fn recover_stale_staging(bundle_root: &Path, staging: &Path, recovery_marker: &Path) -> Result<()> {
    let recovery = match PublisherExchangeRecovery::load(recovery_marker)? {
        Some(recovery) => recovery,
        None => return Ok(()),
    };

    if recovery.exchange_committed(bundle_root, staging)? {
        restore_floor_excluded_content(staging, bundle_root, Path::new("")).with_context(|| {
            format!(
                "restore excluded content from committed publisher generation {}",
                staging.display()
            )
        })?;
    } else if !recovery.exchange_not_committed(bundle_root, staging)? {
        bail!(
            "publisher exchange recovery identities do not match live {} and staging {}; refusing ambiguous recovery",
            bundle_root.display(),
            staging.display()
        );
    }

    lillux::remove_file_durable(recovery_marker)
        .context("retire recovered publisher exchange marker")?;
    Ok(())
}

fn retire_orphan_recovery_marker(recovery_marker: &Path) -> Result<()> {
    match fs::symlink_metadata(recovery_marker) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
                bail!(
                    "publisher exchange recovery marker {} is not a real file",
                    recovery_marker.display()
                );
            }
            lillux::remove_file_durable(recovery_marker)
                .context("retire orphan publisher exchange marker")?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "inspect publisher exchange recovery marker {}",
                    recovery_marker.display()
                )
            });
        }
    }
    Ok(())
}

impl PublisherExchangeRecovery {
    fn capture(live: &Path, staging: &Path) -> Result<Self> {
        let (live_device, live_inode) = publisher_tree_identity(live)?;
        let (staging_device, staging_inode) = publisher_tree_identity(staging)?;
        Ok(Self {
            live_device,
            live_inode,
            staging_device,
            staging_inode,
        })
    }

    fn load(path: &Path) -> Result<Option<Self>> {
        let metadata = match fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "inspect publisher exchange recovery marker {}",
                        path.display()
                    )
                });
            }
        };
        if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
            bail!(
                "publisher exchange recovery marker {} is not a real file",
                path.display()
            );
        }
        if metadata.len() > 4096 {
            bail!(
                "publisher exchange recovery marker {} exceeds 4096 bytes",
                path.display()
            );
        }
        let bytes = fs::read(path).with_context(|| {
            format!("read publisher exchange recovery marker {}", path.display())
        })?;
        let recovery = serde_json::from_slice(&bytes).with_context(|| {
            format!(
                "decode publisher exchange recovery marker {}",
                path.display()
            )
        })?;
        Ok(Some(recovery))
    }

    fn exchange_committed(&self, live: &Path, staging: &Path) -> Result<bool> {
        Ok(
            publisher_tree_identity(live)? == (self.staging_device, self.staging_inode)
                && publisher_tree_identity(staging)? == (self.live_device, self.live_inode),
        )
    }

    fn exchange_not_committed(&self, live: &Path, staging: &Path) -> Result<bool> {
        Ok(
            publisher_tree_identity(live)? == (self.live_device, self.live_inode)
                && publisher_tree_identity(staging)? == (self.staging_device, self.staging_inode),
        )
    }
}

fn publisher_tree_identity(path: &Path) -> Result<(u64, u64)> {
    require_real_directory(path, "publisher generation")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;

        let metadata = fs::symlink_metadata(path)
            .with_context(|| format!("inspect publisher generation {}", path.display()))?;
        return Ok((metadata.dev(), metadata.ino()));
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        bail!("publisher exchange recovery identity is unavailable on this platform")
    }
}

fn ensure_no_floor_excluded_content(root: &Path, relative_root: &Path) -> Result<()> {
    for entry in sorted_dir_entries(&root.join(relative_root))? {
        let relative = relative_root.join(entry.file_name());
        let relative_string = canonical_publisher_relative_path(&relative)?;
        if ryeos_state::project_sync::is_durable_content_capture_floor_excluded(&relative_string) {
            bail!("publisher authoring created floor-excluded content at {relative_string}");
        }
        if entry.file_type()?.is_dir() {
            ensure_no_floor_excluded_content(root, &relative)?;
        }
    }
    Ok(())
}

fn restore_floor_excluded_content(
    old_root: &Path,
    live_root: &Path,
    relative_root: &Path,
) -> Result<()> {
    let old_parent = old_root.join(relative_root);
    if !old_parent.is_dir() {
        return Ok(());
    }
    for entry in sorted_dir_entries(&old_parent)? {
        let relative = relative_root.join(entry.file_name());
        let relative_string = canonical_publisher_relative_path(&relative)?;
        let old_path = entry.path();
        let live_path = live_root.join(&relative);
        if ryeos_state::project_sync::is_durable_content_capture_floor_excluded(&relative_string) {
            if fs::symlink_metadata(&live_path).is_ok() {
                continue;
            }
            fs::rename(&old_path, &live_path).with_context(|| {
                format!(
                    "restore publisher-excluded path {} -> {}",
                    old_path.display(),
                    live_path.display()
                )
            })?;
            continue;
        }
        if entry.file_type()?.is_dir() {
            restore_floor_excluded_content(old_root, live_root, &relative)?;
        }
    }
    Ok(())
}

fn canonical_publisher_relative_path(relative: &Path) -> Result<String> {
    relative
        .to_str()
        .ok_or_else(|| anyhow!("publisher input path is not valid UTF-8"))
        .map(|value| value.replace('\\', "/"))
}

fn copy_tree_contents(source: &Path, destination: &Path, relative_root: &Path) -> Result<()> {
    let metadata =
        fs::symlink_metadata(source).with_context(|| format!("inspect {}", source.display()))?;
    for entry in sorted_dir_entries(source)? {
        let relative = relative_root.join(entry.file_name());
        let relative_string = canonical_publisher_relative_path(&relative)?;
        if ryeos_state::project_sync::is_durable_content_capture_floor_excluded(&relative_string) {
            continue;
        }
        copy_tree_entry(
            &entry.path(),
            &destination.join(entry.file_name()),
            &relative,
        )?;
    }
    preserve_timestamps(destination, &metadata)?;
    fs::set_permissions(destination, metadata.permissions())
        .with_context(|| format!("set permissions on {}", destination.display()))?;
    Ok(())
}

fn copy_tree_entry(source: &Path, destination: &Path, relative: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(source)
        .with_context(|| format!("inspect publisher input {}", source.display()))?;
    let file_type = metadata.file_type();
    if file_type.is_symlink() {
        bail!(
            "publisher input contains a symlink at {}; publish requires a self-contained tree",
            source.display()
        );
    }
    if file_type.is_dir() {
        fs::create_dir(destination)
            .with_context(|| format!("create staged directory {}", destination.display()))?;
        copy_tree_contents(source, destination, relative)?;
        return Ok(());
    }
    if file_type.is_file() {
        fs::copy(source, destination).with_context(|| {
            format!(
                "copy publisher input {} -> {}",
                source.display(),
                destination.display()
            )
        })?;
        preserve_timestamps(destination, &metadata)?;
        return Ok(());
    }

    bail!(
        "publisher input contains a non-regular filesystem entry at {}",
        source.display()
    )
}

/// `fs::copy` preserves permissions but not timestamps. Keeping timestamps for
/// untouched files is part of the publisher contract: bundle preflight rejects
/// source files newer than their signed manifest, and an idempotent publish
/// must not make every source look newly modified merely because it was staged.
fn preserve_timestamps(destination: &Path, source_metadata: &fs::Metadata) -> Result<()> {
    let times =
        fs::FileTimes::new()
            .set_accessed(source_metadata.accessed().with_context(|| {
                format!("read source access time for {}", destination.display())
            })?)
            .set_modified(source_metadata.modified().with_context(|| {
                format!(
                    "read source modification time for {}",
                    destination.display()
                )
            })?);
    fs::File::open(destination)
        .with_context(|| format!("open staged path {}", destination.display()))?
        .set_times(times)
        .with_context(|| format!("preserve timestamps on {}", destination.display()))?;
    Ok(())
}

fn sorted_dir_entries(path: &Path) -> Result<Vec<fs::DirEntry>> {
    let mut entries = fs::read_dir(path)
        .with_context(|| format!("read {}", path.display()))?
        .collect::<std::io::Result<Vec<_>>>()
        .with_context(|| format!("read {}", path.display()))?;
    entries.sort_by_key(fs::DirEntry::file_name);
    Ok(entries)
}

fn require_real_directory(path: &Path, label: &str) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("inspect {label} {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.file_type().is_dir() {
        bail!("{label} {} must be a real directory", path.display());
    }
    Ok(())
}

struct StagingCleanup {
    path: Option<PathBuf>,
}

impl StagingCleanup {
    fn new(path: PathBuf) -> Self {
        Self { path: Some(path) }
    }

    fn disarm(&mut self) {
        self.path = None;
    }
}

impl Drop for StagingCleanup {
    fn drop(&mut self) {
        if let Some(path) = self.path.take() {
            let _ = fs::remove_dir_all(path);
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    fn write_recovery_marker(
        parent: &Path,
        bundle_name: &str,
        recovery: PublisherExchangeRecovery,
    ) {
        fs::create_dir_all(parent.join(".publisher-locks")).unwrap();
        lillux::atomic_write_private(
            &publisher_recovery_marker(parent, bundle_name),
            &serde_json::to_vec(&recovery).unwrap(),
        )
        .unwrap();
    }

    fn sibling_staging_entries(parent: &Path, bundle_name: &str) -> Vec<PathBuf> {
        let name = format!(".{bundle_name}.publish-staging");
        sorted_dir_entries(parent)
            .unwrap()
            .into_iter()
            .filter(|entry| entry.file_name().to_string_lossy() == name.as_str())
            .map(|entry| entry.path())
            .collect()
    }

    fn publisher_lock_anchor(parent: &Path, bundle_name: &str) -> PathBuf {
        parent
            .join(".publisher-locks")
            .join(format!(".{bundle_name}.publish.lock"))
    }

    #[test]
    fn failed_authoring_leaves_the_live_generation_unchanged() {
        let temp = tempfile::tempdir().unwrap();
        let bundle = temp.path().join("bundle");
        fs::create_dir(&bundle).unwrap();
        fs::write(bundle.join("first"), b"old-first").unwrap();
        fs::write(bundle.join("second"), b"old-second").unwrap();

        let error = with_staged_bundle_generation(&bundle, |staging| -> Result<()> {
            fs::write(staging.join("first"), b"new-first")?;
            fs::remove_file(staging.join("second"))?;
            fs::write(staging.join("third"), b"new-third")?;
            bail!("simulated late publisher failure")
        })
        .expect_err("failed authoring must not commit");

        assert!(
            error
                .to_string()
                .contains("simulated late publisher failure")
        );
        assert_eq!(fs::read(bundle.join("first")).unwrap(), b"old-first");
        assert_eq!(fs::read(bundle.join("second")).unwrap(), b"old-second");
        assert!(!bundle.join("third").exists());
        assert!(sibling_staging_entries(temp.path(), "bundle").is_empty());
        assert!(publisher_lock_anchor(temp.path(), "bundle").is_file());
        assert!(!temp.path().join(".bundle.lock").exists());
    }

    #[test]
    fn successful_authoring_replaces_the_complete_generation() {
        let temp = tempfile::tempdir().unwrap();
        let bundle = temp.path().join("bundle");
        fs::create_dir(&bundle).unwrap();
        fs::write(bundle.join("first"), b"old-first").unwrap();
        fs::write(bundle.join("second"), b"old-second").unwrap();
        fs::write(bundle.join("untouched"), b"stable").unwrap();
        let untouched_mtime = fs::metadata(bundle.join("untouched"))
            .unwrap()
            .modified()
            .unwrap();

        let value = with_staged_bundle_generation(&bundle, |staging| {
            fs::write(staging.join("first"), b"new-first")?;
            fs::remove_file(staging.join("second"))?;
            fs::write(staging.join("third"), b"new-third")?;
            Ok(42)
        })
        .expect("complete staged generation should commit");

        assert_eq!(value, 42);
        assert_eq!(fs::read(bundle.join("first")).unwrap(), b"new-first");
        assert!(!bundle.join("second").exists());
        assert_eq!(fs::read(bundle.join("third")).unwrap(), b"new-third");
        assert_eq!(
            fs::metadata(bundle.join("untouched"))
                .unwrap()
                .modified()
                .unwrap(),
            untouched_mtime,
            "staging must not make unchanged source newer than its signed manifest"
        );
        assert!(sibling_staging_entries(temp.path(), "bundle").is_empty());
    }

    #[test]
    fn publisher_staging_uses_the_shared_durable_capture_floor() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let bundle = temp.path().join("bundle");
        fs::create_dir_all(bundle.join(".venv/bin")).unwrap();
        fs::create_dir_all(bundle.join("src")).unwrap();
        fs::write(bundle.join("src/item"), b"authored").unwrap();
        symlink("/usr/bin/python", bundle.join(".venv/bin/python")).unwrap();

        with_staged_bundle_generation(&bundle, |staging| {
            assert!(!staging.join(".venv").exists());
            assert_eq!(fs::read(staging.join("src/item"))?, b"authored");
            Ok(())
        })
        .expect("floor-excluded dependency trees must not enter publisher staging");

        assert!(bundle.join(".venv/bin/python").is_symlink());
        assert_eq!(fs::read(bundle.join("src/item")).unwrap(), b"authored");
    }

    #[test]
    fn next_publish_recovers_a_stale_private_staging_tree() {
        let temp = tempfile::tempdir().unwrap();
        let bundle = temp.path().join("bundle");
        let stale = temp.path().join(".bundle.publish-staging");
        fs::create_dir(&bundle).unwrap();
        fs::write(bundle.join("generation"), b"old").unwrap();
        fs::create_dir(&stale).unwrap();
        fs::write(stale.join("partial"), b"abandoned").unwrap();

        with_staged_bundle_generation(&bundle, |staging| {
            assert!(
                !staging.join("partial").exists(),
                "abandoned publisher output must not enter the next generation"
            );
            fs::write(staging.join("generation"), b"new")?;
            Ok(())
        })
        .expect("stale private staging should be recoverable");

        assert_eq!(fs::read(bundle.join("generation")).unwrap(), b"new");
        assert!(!stale.exists());
    }

    #[test]
    fn committed_exchange_recovery_restores_local_excluded_content() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let bundle = temp.path().join("bundle");
        let staging = temp.path().join(".bundle.publish-staging");
        fs::create_dir_all(bundle.join(".venv/bin")).unwrap();
        fs::write(bundle.join("generation"), b"old").unwrap();
        symlink("/usr/bin/python", bundle.join(".venv/bin/python")).unwrap();
        fs::create_dir(&staging).unwrap();
        fs::write(staging.join("generation"), b"committed").unwrap();

        let recovery = PublisherExchangeRecovery::capture(&bundle, &staging).unwrap();
        write_recovery_marker(temp.path(), "bundle", recovery);
        lillux::atomic_exchange_paths(&bundle, &staging).unwrap();

        with_staged_bundle_generation(&bundle, |next| {
            assert_eq!(fs::read(next.join("generation"))?, b"committed");
            assert!(!next.join(".venv").exists());
            fs::write(next.join("generation"), b"next")?;
            Ok(())
        })
        .expect("next publish should recover the committed exchange first");

        assert_eq!(fs::read(bundle.join("generation")).unwrap(), b"next");
        assert!(bundle.join(".venv/bin/python").is_symlink());
        assert!(!publisher_recovery_marker(temp.path(), "bundle").exists());
    }

    #[test]
    fn uncommitted_exchange_recovery_discards_excluded_staging_content() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let bundle = temp.path().join("bundle");
        let staging = temp.path().join(".bundle.publish-staging");
        fs::create_dir(&bundle).unwrap();
        fs::write(bundle.join("generation"), b"live").unwrap();
        fs::create_dir_all(staging.join(".venv/bin")).unwrap();
        fs::write(staging.join("generation"), b"never-committed").unwrap();
        symlink("/usr/bin/python", staging.join(".venv/bin/python")).unwrap();

        let recovery = PublisherExchangeRecovery::capture(&bundle, &staging).unwrap();
        write_recovery_marker(temp.path(), "bundle", recovery);

        with_staged_bundle_generation(&bundle, |next| {
            assert_eq!(fs::read(next.join("generation"))?, b"live");
            assert!(!next.join(".venv").exists());
            fs::write(next.join("generation"), b"next")?;
            Ok(())
        })
        .expect("uncommitted staging should be discarded before the next publish");

        assert_eq!(fs::read(bundle.join("generation")).unwrap(), b"next");
        assert!(!bundle.join(".venv").exists());
        assert!(!publisher_recovery_marker(temp.path(), "bundle").exists());
    }
}
