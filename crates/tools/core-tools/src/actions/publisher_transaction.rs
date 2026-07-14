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

use anyhow::{anyhow, bail, Context, Result};

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
    // Serialize the read-copy-author-exchange sequence. Without this lock two
    // publishers can both branch from the same old generation and the later
    // exchange silently discard the first publisher's complete result.
    let _lock = lillux::ExclusiveFileLock::acquire(&bundle_root)
        .with_context(|| format!("lock publisher bundle {}", bundle_root.display()))?;
    let parent = bundle_root
        .parent()
        .ok_or_else(|| anyhow!("publisher bundle root has no parent"))?;
    require_real_directory(parent, "publisher bundle parent")?;

    let staging = create_staging_directory(parent, &bundle_root)?;
    let mut cleanup = StagingCleanup::new(staging.clone());

    copy_tree_contents(&bundle_root, &staging).with_context(|| {
        format!(
            "stage publisher generation {} -> {}",
            bundle_root.display(),
            staging.display()
        )
    })?;

    let result = author(&staging)?;
    lillux::sync_tree_durable(&staging)
        .with_context(|| format!("flush staged publisher generation {}", staging.display()))?;

    if let Err(error) = lillux::atomic_exchange_paths(&bundle_root, &staging) {
        if !error.namespace_committed() {
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

fn create_staging_directory(parent: &Path, bundle_root: &Path) -> Result<PathBuf> {
    let name = bundle_root
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow!("publisher bundle root has no UTF-8 directory name"))?;

    let staging = parent.join(format!(".{name}.publish-staging"));
    match fs::symlink_metadata(&staging) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.file_type().is_dir() {
                bail!(
                    "stale publisher staging {} is not a real directory",
                    staging.display()
                );
            }
            lillux::remove_dir_all_durable(&staging)
                .with_context(|| format!("remove stale publisher staging {}", staging.display()))?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error)
                .with_context(|| format!("inspect publisher staging {}", staging.display()))
        }
    }
    fs::create_dir(&staging)
        .with_context(|| format!("create publisher staging {}", staging.display()))?;
    Ok(staging)
}

fn copy_tree_contents(source: &Path, destination: &Path) -> Result<()> {
    let metadata =
        fs::symlink_metadata(source).with_context(|| format!("inspect {}", source.display()))?;
    for entry in sorted_dir_entries(source)? {
        copy_tree_entry(&entry.path(), &destination.join(entry.file_name()))?;
    }
    preserve_timestamps(destination, &metadata)?;
    fs::set_permissions(destination, metadata.permissions())
        .with_context(|| format!("set permissions on {}", destination.display()))?;
    Ok(())
}

fn copy_tree_entry(source: &Path, destination: &Path) -> Result<()> {
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
        copy_tree_contents(source, destination)?;
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

    fn sibling_staging_entries(parent: &Path, bundle_name: &str) -> Vec<PathBuf> {
        let name = format!(".{bundle_name}.publish-staging");
        sorted_dir_entries(parent)
            .unwrap()
            .into_iter()
            .filter(|entry| entry.file_name().to_string_lossy() == name.as_str())
            .map(|entry| entry.path())
            .collect()
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

        assert!(error
            .to_string()
            .contains("simulated late publisher failure"));
        assert_eq!(fs::read(bundle.join("first")).unwrap(), b"old-first");
        assert_eq!(fs::read(bundle.join("second")).unwrap(), b"old-second");
        assert!(!bundle.join("third").exists());
        assert!(sibling_staging_entries(temp.path(), "bundle").is_empty());
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
}
