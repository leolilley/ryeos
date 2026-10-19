//! Retained regular-file observations of matching process descriptors.
//!
//! These snapshots do not identify a descriptor's owner or a particular
//! connection. Concurrent opens, closes and descriptor reuse can affect the
//! result. An observation is neither custody nor an absence/authority proof.

use std::collections::BTreeSet;
use std::fs::File;

use anyhow::Result;

use super::PinnedRegularFileIdentity;

/// Retains the exact file while keeping process descriptor numbers private.
/// No serialization, descriptor adoption or ownership transfer is provided.
pub struct RetainedFileDescriptorObservation {
    retained_file: File,
    identity: PinnedRegularFileIdentity,
    captured: BTreeSet<i32>,
}

impl std::fmt::Debug for RetainedFileDescriptorObservation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RetainedFileDescriptorObservation")
            .finish_non_exhaustive()
    }
}

impl RetainedFileDescriptorObservation {
    /// Capture matching descriptors after retaining the exact regular file.
    /// Linux /proc observation failures propagate; unsupported targets refuse.
    pub fn capture(file: &File) -> Result<Self> {
        #[cfg(not(target_os = "linux"))]
        {
            let _ = file;
            anyhow::bail!("process descriptor observation is unavailable on this platform");
        }
        #[cfg(target_os = "linux")]
        {
            use anyhow::Context as _;
            let identity = super::pinned_regular_file_identity(file)?;
            let retained_file = file.try_clone().context("retain observed regular file")?;
            if !super::matches_pinned_regular_file_identity(&retained_file, identity)? {
                anyhow::bail!("retained descriptor does not identify the observed regular file");
            }
            let captured = matching_descriptors(identity)?;
            Ok(Self {
                retained_file,
                identity,
                captured,
            })
        }
    }

    /// Observe a new matching descriptor number, without assigning its owner.
    /// False does not prove a particular connection failed to retain its file;
    /// true may be an unrelated concurrent open. Neither grants authority.
    pub fn observe_additional_matching_descriptor(&self) -> Result<bool> {
        #[cfg(not(target_os = "linux"))]
        {
            anyhow::bail!("process descriptor observation is unavailable on this platform");
        }
        #[cfg(target_os = "linux")]
        {
            if !super::matches_pinned_regular_file_identity(&self.retained_file, self.identity)? {
                anyhow::bail!("observed retained regular-file identity changed");
            }
            let current = matching_descriptors(self.identity)?;
            Ok(!current.is_subset(&self.captured))
        }
    }
}

#[cfg(target_os = "linux")]
fn matching_descriptors(expected: PinnedRegularFileIdentity) -> Result<BTreeSet<i32>> {
    use anyhow::Context as _;
    use std::os::unix::fs::MetadataExt as _;

    let mut descriptors = BTreeSet::new();
    for entry in std::fs::read_dir("/proc/self/fd").context("enumerate process descriptors")? {
        let entry = entry.context("read process descriptor entry")?;
        let Some(descriptor) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<i32>().ok())
        else {
            continue;
        };
        let metadata = match std::fs::metadata(entry.path()) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error).context("observe process descriptor identity"),
        };
        if metadata.is_file()
            && metadata.dev() == expected.containing_device
            && metadata.ino() == expected.inode
        {
            descriptors.insert(descriptor);
        }
    }
    Ok(descriptors)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "linux")]
    #[test]
    fn observes_additional_duplicate_and_retains_file_after_original_closes() {
        let file = tempfile::tempfile().unwrap();
        let observation = RetainedFileDescriptorObservation::capture(&file).unwrap();
        assert!(
            !observation
                .observe_additional_matching_descriptor()
                .unwrap()
        );
        let duplicate = file.try_clone().unwrap();
        drop(file);
        assert!(
            observation
                .observe_additional_matching_descriptor()
                .unwrap()
        );
        drop(duplicate);
        assert!(
            !observation
                .observe_additional_matching_descriptor()
                .unwrap()
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn different_inode_is_not_an_additional_matching_descriptor() {
        let file = tempfile::tempfile().unwrap();
        let observation = RetainedFileDescriptorObservation::capture(&file).unwrap();
        let unrelated = tempfile::tempfile().unwrap();
        assert!(
            !observation
                .observe_additional_matching_descriptor()
                .unwrap()
        );
        drop(unrelated);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn rejects_nonregular_input() {
        let directory = tempfile::TempDir::new().unwrap();
        let file = File::open(directory.path()).unwrap();
        assert!(RetainedFileDescriptorObservation::capture(&file).is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn concurrent_unrelated_open_can_produce_observation_without_connection_custody() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let observation = RetainedFileDescriptorObservation::capture(file.as_file()).unwrap();
        let path = file.path().to_owned();
        let (opened_tx, opened_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let unrelated_owner = File::open(path).unwrap();
            opened_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            drop(unrelated_owner);
        });
        opened_rx.recv().unwrap();
        let result = observation.observe_additional_matching_descriptor();
        release_tx.send(()).unwrap();
        worker.join().unwrap();
        assert!(result.unwrap());
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn unsupported_platform_refuses_instead_of_empty_success() {
        let file = tempfile::tempfile().unwrap();
        let error = RetainedFileDescriptorObservation::capture(&file).unwrap_err();
        assert!(error.to_string().contains("unavailable"));
    }
}
