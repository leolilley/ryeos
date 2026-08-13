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

const PUBLISHER_TRAVERSAL: lillux::DirectoryTraversalBudget =
    lillux::DirectoryTraversalBudget::new(1_000_000, 128);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PublisherExchangeRecovery {
    live_identity: lillux::PinnedDirectoryIdentity,
    staging_identity: lillux::PinnedDirectoryIdentity,
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

    let pinned_parent = lillux::PinnedDirectory::open(parent)?
        .ok_or_else(|| anyhow!("publisher bundle parent is unavailable"))?;
    let (staging, pinned_staging) = create_staging_directory(&pinned_parent, &bundle_root)?;
    let recovery_marker = publisher_recovery_marker(parent, bundle_name);
    let pinned_live = pinned_parent
        .open_child_directory(bundle_root.file_name().expect("bundle name checked"))?
        .ok_or_else(|| anyhow!("publisher live generation is unavailable"))?;
    let mut cleanup = StagingCleanup::new(
        pinned_parent.try_clone()?,
        staging
            .file_name()
            .expect("staging name checked")
            .to_owned(),
        pinned_staging.try_clone()?,
    );

    pinned_live
        .copy_contents_to_filtered(&pinned_staging, PUBLISHER_TRAVERSAL, |relative| {
            let relative = canonical_publisher_relative_path(relative)?;
            Ok(ryeos_state::project_sync::is_durable_content_capture_floor_excluded(&relative))
        })
        .with_context(|| {
            format!(
                "stage publisher generation {} -> {}",
                bundle_root.display(),
                staging.display()
            )
        })?;

    let result = author(&staging)?;
    ensure_no_floor_excluded_content(&staging, Path::new(""))?;
    pinned_staging
        .sync_tree_bounded(PUBLISHER_TRAVERSAL)
        .with_context(|| format!("flush staged publisher generation {}", staging.display()))?;
    pinned_live.ensure_path_binding()?;
    pinned_staging.ensure_path_binding()?;
    let recovery = PublisherExchangeRecovery {
        live_identity: pinned_live.identity()?,
        staging_identity: pinned_staging.identity()?,
    };
    lillux::atomic_write_private(
        &recovery_marker,
        &serde_json::to_vec(&recovery).context("serialize publisher exchange recovery marker")?,
    )
    .context("publish publisher exchange recovery marker")?;

    if let Err(error) = pinned_parent.exchange_child_directories_if_same(
        bundle_root.file_name().expect("bundle name checked"),
        &pinned_live,
        staging.file_name().expect("staging name checked"),
        &pinned_staging,
    ) {
        if error.namespace_requires_recovery() {
            cleanup.disarm();
            return Err(error).with_context(|| {
                format!(
                    "publisher exchange committed an ambiguous namespace; recovery marker retained at {}",
                    recovery_marker.display()
                )
            });
        }
        if !error.namespace_committed() {
            let _ = remove_recovery_marker_exact(&recovery_marker);
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
        if let Err(sync_error) = pinned_parent.sync() {
            cleanup.disarm();
            return Err(anyhow!(
                "publisher generation was committed, but the parent-directory durability barrier remains uncertain after retry; recovery marker retained at {} and the publication must be recovered before retrying: {error}; {sync_error:#}",
                recovery_marker.display()
            ));
        }
    }
    // The pinned staging descriptor now names the committed live generation;
    // pre-commit cleanup must never touch it after the exchange boundary.
    cleanup.disarm();

    if let Err(error) = restore_floor_excluded_content(&staging, &bundle_root, Path::new("")) {
        tracing::warn!(
            old_generation = %staging.display(),
            live_generation = %bundle_root.display(),
            error = %error,
            "publisher generation committed but local excluded content remains in the recoverable old generation"
        );
        cleanup.disarm();
        return Err(error).with_context(|| {
            format!(
                "publisher generation committed, but local excluded content was not restored; recovery marker retained at {} and the publication must be recovered before retrying",
                recovery_marker.display()
            )
        });
    }
    if let Err(error) = remove_recovery_marker_exact(&recovery_marker) {
        tracing::warn!(
            path = %recovery_marker.display(),
            error = %error,
            "publisher generation committed but recovery-marker cleanup failed"
        );
    }
    match pinned_parent.open_child_directory(staging.file_name().expect("staging name checked")) {
        Ok(Some(old_generation)) => {
            if let Err(error) = old_generation
                .remove_contents_recursive_bounded(PUBLISHER_TRAVERSAL)
                .and_then(|_| {
                    pinned_parent
                        .remove_empty_child_if_same(
                            staging.file_name().expect("staging name checked"),
                            &old_generation,
                        )
                        .and_then(|removed| {
                            if removed {
                                Ok(())
                            } else {
                                bail!("old publisher generation remained non-empty")
                            }
                        })
                })
            {
                tracing::warn!(
                    path = %staging.display(),
                    error = %error,
                    "publisher generation committed but previous-generation cleanup failed"
                );
            }
        }
        Ok(None) => {}
        Err(error) => tracing::warn!(
            path = %staging.display(),
            error = %error,
            "publisher generation committed but previous-generation cleanup could not be inspected"
        ),
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

fn create_staging_directory(
    parent: &lillux::PinnedDirectory,
    bundle_root: &Path,
) -> Result<(PathBuf, lillux::PinnedDirectory)> {
    let name = bundle_root
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow!("publisher bundle root has no UTF-8 directory name"))?;

    let staging_name = std::ffi::OsString::from(format!(".{name}.publish-staging"));
    let staging = parent.path().join(&staging_name);
    let recovery_marker = publisher_recovery_marker(parent.path(), name);
    match parent.entry_no_follow(&staging_name)? {
        Some(entry) => {
            if entry.entry_type != lillux::PinnedEntryType::Directory {
                bail!(
                    "stale publisher staging {} is not a real directory",
                    staging.display()
                );
            }
            recover_stale_staging(bundle_root, &staging, &recovery_marker)?;
            let stale = parent
                .open_child_directory(&staging_name)?
                .ok_or_else(|| anyhow!("stale publisher staging disappeared"))?;
            stale.ensure_path_binding()?;
            stale.remove_contents_recursive_bounded(PUBLISHER_TRAVERSAL)?;
            if !parent.remove_empty_child_if_same(&staging_name, &stale)? {
                bail!("stale publisher staging remained non-empty");
            }
        }
        None => {
            remove_recovery_marker_exact(&recovery_marker)?;
        }
    }
    let pinned = parent.create_child(&staging_name, 0o755)?;
    Ok((staging, pinned))
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

    remove_recovery_marker_exact(recovery_marker)
        .context("retire recovered publisher exchange marker")?;
    Ok(())
}

fn remove_recovery_marker_exact(recovery_marker: &Path) -> Result<()> {
    let Some(parent_path) = recovery_marker.parent() else {
        bail!("publisher recovery marker has no parent");
    };
    let Some(parent) = lillux::PinnedDirectory::open(parent_path)? else {
        return Ok(());
    };
    let name = recovery_marker
        .file_name()
        .ok_or_else(|| anyhow!("publisher recovery marker has no file name"))?;
    if let Some(file) = parent.open_regular(name, false)? {
        parent
            .remove_if_same_atomic(name, &file)
            .map_err(|error| anyhow!(error))?;
    }
    Ok(())
}

impl PublisherExchangeRecovery {
    #[cfg(all(test, target_os = "linux"))]
    fn capture(live: &Path, staging: &Path) -> Result<Self> {
        Ok(Self {
            live_identity: publisher_tree_identity(live)?,
            staging_identity: publisher_tree_identity(staging)?,
        })
    }

    fn load(path: &Path) -> Result<Option<Self>> {
        let Some(bytes) = lillux::read_optional_regular_file_bounded_no_follow(path, 4096)? else {
            return Ok(None);
        };
        let recovery = serde_json::from_slice(&bytes).with_context(|| {
            format!(
                "decode publisher exchange recovery marker {}",
                path.display()
            )
        })?;
        Ok(Some(recovery))
    }

    fn exchange_committed(&self, live: &Path, staging: &Path) -> Result<bool> {
        Ok(publisher_tree_identity(live)? == self.staging_identity
            && publisher_tree_identity(staging)? == self.live_identity)
    }

    fn exchange_not_committed(&self, live: &Path, staging: &Path) -> Result<bool> {
        Ok(publisher_tree_identity(live)? == self.live_identity
            && publisher_tree_identity(staging)? == self.staging_identity)
    }
}

fn publisher_tree_identity(path: &Path) -> Result<lillux::PinnedDirectoryIdentity> {
    require_real_directory(path, "publisher generation")?;
    let directory = lillux::PinnedDirectory::open(path)?
        .ok_or_else(|| anyhow!("publisher generation is unavailable"))?;
    directory.identity()
}

fn ensure_no_floor_excluded_content(root: &Path, relative_root: &Path) -> Result<()> {
    let root = lillux::PinnedDirectory::open(root)?
        .ok_or_else(|| anyhow!("staged publisher generation is unavailable"))?;
    let mut remaining = PUBLISHER_TRAVERSAL.max_entries;
    ensure_no_floor_excluded_content_open(&root, relative_root, 0, &mut remaining)?;
    root.ensure_path_binding()?;
    Ok(())
}

fn ensure_no_floor_excluded_content_open(
    root: &lillux::PinnedDirectory,
    relative_root: &Path,
    depth: usize,
    remaining: &mut usize,
) -> Result<()> {
    if depth > 128 {
        bail!("publisher tree exceeds its traversal depth bound");
    }
    let entries = root.entries_no_follow_bounded(*remaining)?;
    *remaining = remaining
        .checked_sub(entries.len())
        .ok_or_else(|| anyhow!("publisher validation entry budget underflow"))?;
    for entry in entries {
        let relative = relative_root.join(&entry.name);
        let relative_string = canonical_publisher_relative_path(&relative)?;
        if ryeos_state::project_sync::is_durable_content_capture_floor_excluded(&relative_string) {
            bail!("publisher authoring created floor-excluded content at {relative_string}");
        }
        match entry.entry_type {
            lillux::PinnedEntryType::Directory => {
                let child = root
                    .open_child_directory(&entry.name)?
                    .ok_or_else(|| anyhow!("publisher directory disappeared during validation"))?;
                ensure_no_floor_excluded_content_open(&child, &relative, depth + 1, remaining)?;
                root.ensure_entry_observation(&entry)?;
            }
            lillux::PinnedEntryType::Regular => {
                root.ensure_entry_observation(&entry)?;
            }
            other => bail!(
                "publisher generation contains unsupported {other:?} entry at {relative_string}"
            ),
        }
    }
    Ok(())
}

fn restore_floor_excluded_content(
    old_root: &Path,
    live_root: &Path,
    _relative_root: &Path,
) -> Result<()> {
    let old = lillux::PinnedDirectory::open(old_root)?
        .ok_or_else(|| anyhow!("old publisher generation is unavailable"))?;
    let live = lillux::PinnedDirectory::open(live_root)?
        .ok_or_else(|| anyhow!("live publisher generation is unavailable"))?;
    let mut remaining = PUBLISHER_TRAVERSAL.max_entries;
    restore_floor_excluded_content_open(&old, &live, Path::new(""), 0, &mut remaining)?;
    old.sync()?;
    live.sync()?;
    Ok(())
}

fn restore_floor_excluded_content_open(
    old: &lillux::PinnedDirectory,
    live: &lillux::PinnedDirectory,
    relative_root: &Path,
    depth: usize,
    remaining: &mut usize,
) -> Result<()> {
    if depth > 128 {
        bail!("publisher excluded-content restoration exceeds its depth bound");
    }
    let entries = old.entries_no_follow_bounded(*remaining)?;
    *remaining = remaining
        .checked_sub(entries.len())
        .ok_or_else(|| anyhow!("publisher restoration entry budget underflow"))?;
    for entry in entries {
        let relative = relative_root.join(&entry.name);
        let relative_string = canonical_publisher_relative_path(&relative)?;
        if ryeos_state::project_sync::is_durable_content_capture_floor_excluded(&relative_string) {
            match old.move_child_if_same_noreplace_to(&entry, live) {
                Ok(true) => {}
                Ok(false) => bail!(
                    "publisher excluded-content restoration refused occupied destination at {relative_string}"
                ),
                Err(error) if error.namespace_committed() => {
                    tracing::warn!(
                        path = %relative_string,
                        error = %error,
                        "publisher excluded content was restored but directory durability is uncertain"
                    );
                    old.sync().with_context(|| {
                        format!(
                            "excluded content was restored at {relative_string}, but the old nested parent remains durability-uncertain"
                        )
                    })?;
                    live.sync().with_context(|| {
                        format!(
                            "excluded content was restored at {relative_string}, but the live nested parent remains durability-uncertain"
                        )
                    })?;
                }
                Err(error) => return Err(anyhow!(error)),
            }
            continue;
        }
        if entry.entry_type == lillux::PinnedEntryType::Directory {
            let old_child = old
                .open_child_directory(&entry.name)?
                .ok_or_else(|| anyhow!("old publisher directory disappeared during restoration"))?;
            let live_child = live
                .open_child_directory(&entry.name)?
                .ok_or_else(|| anyhow!("live publisher directory is missing during restoration"))?;
            restore_floor_excluded_content_open(
                &old_child,
                &live_child,
                &relative,
                depth + 1,
                remaining,
            )?;
            old.ensure_entry_observation(&entry)?;
        }
    }
    // Every recursion frame is a recovery boundary. An excluded entry may
    // already have moved during a prior committed-but-uncertain attempt, so a
    // later recovery cannot rely on seeing that entry again to rediscover the
    // two parent directories whose rename must be sealed.
    old.sync()?;
    live.sync()?;
    Ok(())
}

fn canonical_publisher_relative_path(relative: &Path) -> Result<String> {
    relative
        .to_str()
        .ok_or_else(|| anyhow!("publisher input path is not valid UTF-8"))
        .map(|value| value.replace('\\', "/"))
}

#[cfg(all(test, target_os = "linux"))]
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
    parent: lillux::PinnedDirectory,
    name: std::ffi::OsString,
    directory: lillux::PinnedDirectory,
    armed: bool,
}

impl StagingCleanup {
    fn new(
        parent: lillux::PinnedDirectory,
        name: std::ffi::OsString,
        directory: lillux::PinnedDirectory,
    ) -> Self {
        Self {
            parent,
            name,
            directory,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for StagingCleanup {
    fn drop(&mut self) {
        if self.armed {
            let _ = self
                .directory
                .remove_contents_recursive_bounded(PUBLISHER_TRAVERSAL);
            let _ = self
                .parent
                .remove_empty_child_if_same(&self.name, &self.directory);
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
