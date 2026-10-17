//! Bounded prefix enumeration below one pinned directory.
//!
//! A walk reads directory names, inspects each entry without following links
//! and descends depth-first in byte-wise name order. Every operation is
//! relative to a retained directory descriptor. Three caller budgets bound
//! it: directory levels, reported entries and kernel work.
//!
//! Exhaustion is reported only after the walk actually refused something: an
//! included entry that the entry budget could not admit, or an operation the
//! work budget could not pay for. A budget that is met exactly, with nothing
//! left over, is not exhaustion. Every directory the walk reaches carries a
//! coverage record that states which of its immediate names were read and
//! reported. Callers never infer completeness from the absence of an error.
//!
//! The walk observes a live namespace. An entry that disappears between its
//! name being read and its inspection, a directory entry that names a
//! different inode when opened, and a directory whose metadata changes while
//! it is walked are all recorded rather than hidden. None of them aborts the
//! walk, and none of the affected directories is reported complete.
//!
//! A child directory on a different mount than its parent is reported but
//! never read.
//!
//! Strict whole-directory enumeration remains
//! [`PinnedDirectory::entries_no_follow_bounded`], which refuses a directory
//! over its limit instead of returning a prefix.

use std::ffi::{CString, OsStr, OsString};
use std::fs::File;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};

use super::{
    PinnedDirectory, PinnedDirectoryEntryMetadata, PinnedEntryType, entry_type_from_mode,
    mount_id_of_open_descriptor, read_directory_names,
};

/// Directory levels one walk may read. Each level being walked retains one
/// descriptor, so this bounds descriptor use independently of caller budgets.
pub const MAX_PREFIX_WALK_DEPTH: usize = 64;

/// Work units one walk may be granted. Names read are retained until their
/// directory is finished, so this also bounds the walk's name memory.
pub const MAX_PREFIX_WALK_WORK: usize = 1 << 20;

/// Caller limits for one prefix walk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PrefixWalkBudget {
    /// Directory levels whose names may be read. Zero reads nothing. One
    /// reads only the walk root, and child directories are recorded as
    /// depth-limited.
    pub max_depth: usize,
    /// Included entries that may be reported across the whole walk.
    pub max_entries: usize,
    /// Kernel work units across the whole walk: one per directory opened for
    /// reading, one per child name a directory stream returns and one per
    /// child inspected.
    pub max_work: usize,
}

/// A caller's verdict on one inspected entry. Excluded entries are neither
/// reported nor descended into, and they do not consume the entry budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrefixWalkDecision {
    Include,
    Exclude,
}

/// One reported entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrefixWalkEntry {
    /// Path below the walk root, built only from names the walk read.
    pub relative_path: PathBuf,
    /// Level of the directory that contains this entry; the root is level 0.
    pub depth: usize,
    /// No-follow inspection of the entry.
    pub metadata: PinnedDirectoryEntryMetadata,
    /// Byte length of a regular file from the same inspection.
    pub size: Option<u64>,
}

/// How much of one directory's immediate names a walk observed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrefixDirectoryCoverage {
    /// Every name was read to the end of the directory stream and every name
    /// was inspected.
    Complete,
    /// Every name was read. Inspection stopped part-way, so the reported
    /// entries are a prefix of this directory in byte-wise name order.
    NamePrefix,
    /// The work budget stopped the directory stream before its end. The
    /// reported entries are an arbitrary subset of the directory, presented
    /// in name order.
    StreamPrefix,
    /// The directory lies at the depth limit and was not opened.
    DepthLimit,
    /// A budget was exhausted before the directory was opened.
    NotVisited,
    /// When opened, the name no longer led to the inspected directory inode.
    Replaced,
    /// The directory is a different mount than its parent. It was not read,
    /// so the walk never descends into another filesystem.
    MountBoundary,
    /// Opening or reading the directory failed.
    Failed,
}

/// Coverage for one directory the walk reached: the root, then every
/// reported directory entry in walk order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrefixWalkDirectory {
    /// Path below the walk root; empty for the root.
    pub relative_path: PathBuf,
    /// Level of this directory's own names; the root is level 0.
    pub depth: usize,
    pub coverage: PrefixDirectoryCoverage,
    /// Names read from the directory stream and charged as work.
    pub names_read: usize,
    /// Entries reported from this directory.
    pub reported: usize,
    /// Entries the caller excluded.
    pub excluded: usize,
    /// Names that had disappeared by the time they were inspected.
    pub vanished: usize,
    /// Names whose inspection failed for a reason other than disappearance.
    pub failed_entries: usize,
    /// The directory's metadata changed while it was walked, or one of its
    /// entries vanished or was replaced.
    pub changed: bool,
    /// Error text when the coverage is [`PrefixDirectoryCoverage::Failed`].
    pub failure: Option<String>,
}

impl PrefixWalkDirectory {
    fn unvisited(relative_path: PathBuf, depth: usize) -> Self {
        Self {
            relative_path,
            depth,
            coverage: PrefixDirectoryCoverage::NotVisited,
            names_read: 0,
            reported: 0,
            excluded: 0,
            vanished: 0,
            failed_entries: 0,
            changed: false,
            failure: None,
        }
    }

    /// Every immediate name was observed, and nothing changed or failed.
    pub fn is_complete(&self) -> bool {
        self.coverage == PrefixDirectoryCoverage::Complete
            && !self.changed
            && self.vanished == 0
            && self.failed_entries == 0
    }
}

/// The budget that ended a walk early.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrefixWalkExhaustion {
    /// An included entry was found after the entry budget was spent.
    Entries,
    /// An operation was needed after the work budget was spent, or a
    /// directory stream was cut short to leave work for inspection.
    Work,
}

/// Result of one bounded prefix walk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrefixWalk {
    /// Reported entries in walk order.
    pub entries: Vec<PrefixWalkEntry>,
    /// Coverage records. Index 0 is always the walk root.
    pub directories: Vec<PrefixWalkDirectory>,
    pub exhausted: Option<PrefixWalkExhaustion>,
    pub work_used: usize,
}

impl PrefixWalk {
    /// Coverage of the walk root's immediate names.
    pub fn root(&self) -> &PrefixWalkDirectory {
        &self.directories[0]
    }

    /// Every included entry below the root, at every depth, was reported.
    pub fn is_complete(&self) -> bool {
        self.exhausted.is_none() && self.directories.iter().all(PrefixWalkDirectory::is_complete)
    }
}

impl PinnedDirectory {
    /// Walk this directory within `budget`, reporting a bounded prefix with
    /// exact coverage. `filter` sees each inspected entry before it is
    /// reported. Links are never followed and child directories are opened
    /// relative to their retained parent and checked against the inspected
    /// inode. Every descriptor the walk opens is released before it returns,
    /// on success and on error.
    pub fn walk_prefix_no_follow<F>(
        &self,
        budget: PrefixWalkBudget,
        mut filter: F,
    ) -> Result<PrefixWalk>
    where
        F: FnMut(&Path, &PinnedDirectoryEntryMetadata) -> PrefixWalkDecision,
    {
        ensure!(
            budget.max_depth <= MAX_PREFIX_WALK_DEPTH,
            "prefix walk depth {} exceeds the supported maximum {MAX_PREFIX_WALK_DEPTH}",
            budget.max_depth
        );
        ensure!(
            budget.max_work <= MAX_PREFIX_WALK_WORK,
            "prefix walk work {} exceeds the supported maximum {MAX_PREFIX_WALK_WORK}",
            budget.max_work
        );
        let mut walk = PrefixWalk {
            entries: Vec::new(),
            directories: vec![PrefixWalkDirectory::unvisited(PathBuf::new(), 0)],
            exhausted: None,
            work_used: 0,
        };
        let mut work = WorkMeter {
            used: 0,
            max: budget.max_work,
        };
        if budget.max_depth == 0 {
            walk.directories[0].coverage = PrefixDirectoryCoverage::DepthLimit;
            return Ok(walk);
        }
        if !work.take(1) {
            walk.exhausted = Some(PrefixWalkExhaustion::Work);
            return Ok(walk);
        }
        let root = self
            .directory
            .try_clone()
            .with_context(|| format!("duplicate prefix walk root {}", self.path.display()))?;
        let root = match read_frame(root, PathBuf::new(), 0, 0, &mut work, &mut walk)? {
            FrameRead::Frame(frame) => frame,
            FrameRead::Failed(error) => {
                anyhow::bail!("read prefix walk root {}: {error}", self.path.display())
            }
        };
        let mut stack = vec![root];
        let exhausted = loop {
            let Some(frame) = stack.last_mut() else {
                break None;
            };
            if frame.next >= frame.names.len() {
                let Some(finished) = stack.pop() else {
                    break None;
                };
                let stream_cut = !finished.stream_complete;
                close_frame(finished, &mut walk);
                if stream_cut {
                    break Some(PrefixWalkExhaustion::Work);
                }
                continue;
            }
            if !work.take(1) {
                break Some(PrefixWalkExhaustion::Work);
            }
            let name = frame.names[frame.next].clone();
            let (metadata, size) = match inspect_entry(&frame.directory, &name)? {
                Inspection::Present { metadata, size } => (metadata, size),
                Inspection::Vanished => {
                    let record = &mut walk.directories[frame.record];
                    record.vanished += 1;
                    record.changed = true;
                    frame.next += 1;
                    continue;
                }
                Inspection::Failed => {
                    walk.directories[frame.record].failed_entries += 1;
                    frame.next += 1;
                    continue;
                }
            };
            let relative = frame.relative.join(&name);
            if filter(relative.as_path(), &metadata) == PrefixWalkDecision::Exclude {
                walk.directories[frame.record].excluded += 1;
                frame.next += 1;
                continue;
            }
            if walk.entries.len() >= budget.max_entries {
                break Some(PrefixWalkExhaustion::Entries);
            }
            frame.next += 1;
            walk.directories[frame.record].reported += 1;
            let is_directory = metadata.entry_type == PinnedEntryType::Directory;
            let child_depth = frame.depth + 1;
            walk.entries.push(PrefixWalkEntry {
                relative_path: relative.clone(),
                depth: frame.depth,
                metadata: metadata.clone(),
                size,
            });
            if !is_directory {
                continue;
            }
            let child_record = walk.directories.len();
            walk.directories
                .push(PrefixWalkDirectory::unvisited(relative.clone(), child_depth));
            if !frame.stream_complete {
                // The work budget already cut this directory's stream; the
                // walk ends once its read names are inspected.
                continue;
            }
            if child_depth >= budget.max_depth {
                walk.directories[child_record].coverage = PrefixDirectoryCoverage::DepthLimit;
                continue;
            }
            if !work.take(1) {
                break Some(PrefixWalkExhaustion::Work);
            }
            let child = match open_child_directory(&frame.directory, &name, &metadata)? {
                ChildOpen::Opened(child) => child,
                ChildOpen::Replaced => {
                    walk.directories[child_record].coverage = PrefixDirectoryCoverage::Replaced;
                    walk.directories[frame.record].changed = true;
                    continue;
                }
                ChildOpen::MountBoundary => {
                    walk.directories[child_record].coverage =
                        PrefixDirectoryCoverage::MountBoundary;
                    continue;
                }
                ChildOpen::Failed(error) => {
                    let record = &mut walk.directories[child_record];
                    record.coverage = PrefixDirectoryCoverage::Failed;
                    record.failure = Some(error);
                    continue;
                }
            };
            match read_frame(child, relative, child_depth, child_record, &mut work, &mut walk)? {
                FrameRead::Frame(child) => stack.push(child),
                FrameRead::Failed(error) => {
                    let record = &mut walk.directories[child_record];
                    record.coverage = PrefixDirectoryCoverage::Failed;
                    record.failure = Some(error);
                }
            }
        };
        while let Some(frame) = stack.pop() {
            close_frame(frame, &mut walk);
        }
        walk.exhausted = exhausted;
        walk.work_used = work.used;
        Ok(walk)
    }
}

struct WorkMeter {
    used: usize,
    max: usize,
}

impl WorkMeter {
    fn remaining(&self) -> usize {
        self.max.saturating_sub(self.used)
    }

    fn take(&mut self, units: usize) -> bool {
        if units > self.remaining() {
            return false;
        }
        self.used += units;
        true
    }
}

/// Directory metadata that changes when an entry is added, removed or
/// renamed. Detection is limited by filesystem timestamp granularity, so
/// vanished and replaced entries are also recorded independently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ChangeStamp {
    modified: (i64, i64),
    changed: (i64, i64),
    links: u64,
    size: u64,
}

fn change_stamp(directory: &File) -> Result<ChangeStamp> {
    let metadata = directory
        .metadata()
        .context("inspect prefix walk directory")?;
    Ok(ChangeStamp {
        modified: (metadata.mtime(), metadata.mtime_nsec()),
        changed: (metadata.ctime(), metadata.ctime_nsec()),
        links: metadata.nlink(),
        size: metadata.size(),
    })
}

/// One directory being walked. Dropping it releases its descriptor.
struct Frame {
    directory: File,
    relative: PathBuf,
    depth: usize,
    record: usize,
    names: Vec<OsString>,
    next: usize,
    stream_complete: bool,
    stamp: ChangeStamp,
}

enum FrameRead {
    Frame(Frame),
    Failed(String),
}

/// Read a directory's names. The stream may use at most half of the
/// remaining work, rounded down, so that every name read can still be
/// inspected. Failure to inspect or read the directory is reported as a
/// failed frame rather than an error, so one directory cannot end the walk.
fn read_frame(
    directory: File,
    relative: PathBuf,
    depth: usize,
    record: usize,
    work: &mut WorkMeter,
    walk: &mut PrefixWalk,
) -> Result<FrameRead> {
    let stamp = match change_stamp(&directory) {
        Ok(stamp) => stamp,
        Err(error) => return Ok(FrameRead::Failed(format!("{error:#}"))),
    };
    let allowance = work.remaining() / 2;
    let (mut names, stream_complete) =
        match read_directory_names(&directory, Some(allowance), true, || Ok(())) {
            Ok(read) => read,
            Err(error) => return Ok(FrameRead::Failed(format!("{error:#}"))),
        };
    ensure!(
        work.take(names.len()),
        "prefix walk read more names than its work allowance"
    );
    names.sort_by(|left, right| left.as_bytes().cmp(right.as_bytes()));
    walk.directories[record].names_read = names.len();
    Ok(FrameRead::Frame(Frame {
        directory,
        relative,
        depth,
        record,
        names,
        next: 0,
        stream_complete,
        stamp,
    }))
}

/// Settle a frame's coverage from what was actually read and inspected, then
/// release its descriptor. A directory whose metadata can no longer be
/// observed is recorded as changed.
fn close_frame(frame: Frame, walk: &mut PrefixWalk) {
    let coverage = if !frame.stream_complete {
        PrefixDirectoryCoverage::StreamPrefix
    } else if frame.next < frame.names.len() {
        PrefixDirectoryCoverage::NamePrefix
    } else {
        PrefixDirectoryCoverage::Complete
    };
    let changed = match change_stamp(&frame.directory) {
        Ok(stamp) => stamp != frame.stamp,
        Err(_) => true,
    };
    let record = &mut walk.directories[frame.record];
    record.coverage = coverage;
    if changed {
        record.changed = true;
    }
}

enum Inspection {
    Present {
        metadata: PinnedDirectoryEntryMetadata,
        size: Option<u64>,
    },
    Vanished,
    Failed,
}

fn inspect_entry(directory: &File, name: &OsStr) -> Result<Inspection> {
    let c_name = CString::new(name.as_bytes()).context("directory name contains NUL")?;
    // SAFETY: stat is plain data and is fully written on success.
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: the descriptor is open, the name is NUL-terminated and the
    // output buffer is valid for writes.
    if unsafe {
        libc::fstatat(
            directory.as_raw_fd(),
            c_name.as_ptr(),
            &mut stat,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ENOENT) {
            return Ok(Inspection::Vanished);
        }
        return Ok(Inspection::Failed);
    }
    let entry_type = entry_type_from_mode(stat.st_mode);
    let size = if entry_type == PinnedEntryType::Regular {
        u64::try_from(stat.st_size).ok()
    } else {
        None
    };
    Ok(Inspection::Present {
        metadata: PinnedDirectoryEntryMetadata {
            name: name.to_os_string(),
            entry_type,
            mode: stat.st_mode,
            device_id: stat.st_rdev,
            containing_device: stat.st_dev,
            inode: stat.st_ino,
        },
        size,
    })
}

#[derive(Debug)]
enum ChildOpen {
    Opened(File),
    Replaced,
    MountBoundary,
    Failed(String),
}

/// Classify a failed no-follow directory open. A name that is gone, is no
/// longer a directory or is now a link no longer leads to the inspected
/// directory; every other error is a failure to open it.
fn child_open_failure(error: std::io::Error) -> ChildOpen {
    match error.raw_os_error() {
        Some(libc::ENOENT | libc::ENOTDIR | libc::ELOOP) => ChildOpen::Replaced,
        _ => ChildOpen::Failed(error.to_string()),
    }
}

/// Open an inspected child directory without following links and confirm
/// that the name still leads to the inspected inode.
fn open_child_directory(
    parent: &File,
    name: &OsStr,
    expected: &PinnedDirectoryEntryMetadata,
) -> Result<ChildOpen> {
    let c_name = CString::new(name.as_bytes()).context("directory name contains NUL")?;
    // SAFETY: the parent descriptor is open and the name is NUL-terminated.
    let descriptor = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            c_name.as_ptr(),
            libc::O_RDONLY
                | libc::O_DIRECTORY
                | libc::O_NOFOLLOW
                | libc::O_CLOEXEC
                | libc::O_NONBLOCK,
        )
    };
    if descriptor < 0 {
        return Ok(child_open_failure(std::io::Error::last_os_error()));
    }
    // SAFETY: openat returned a new descriptor that nothing else owns.
    let child = unsafe { File::from_raw_fd(descriptor) };
    let metadata = match child.metadata() {
        Ok(metadata) => metadata,
        Err(error) => return Ok(ChildOpen::Failed(error.to_string())),
    };
    if metadata.dev() != expected.containing_device || metadata.ino() != expected.inode {
        return Ok(ChildOpen::Replaced);
    }
    match (
        mount_id_of_open_descriptor(parent),
        mount_id_of_open_descriptor(&child),
    ) {
        (Ok(parent_mount), Ok(child_mount)) if parent_mount == child_mount => {}
        (Ok(_), Ok(_)) => return Ok(ChildOpen::MountBoundary),
        (Err(error), _) | (_, Err(error)) => return Ok(ChildOpen::Failed(format!("{error:#}"))),
    }
    Ok(ChildOpen::Opened(child))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pinned(path: &Path) -> PinnedDirectory {
        PinnedDirectory::open(path)
            .expect("pin walk root")
            .expect("walk root exists")
    }

    fn budget(max_depth: usize, max_entries: usize, max_work: usize) -> PrefixWalkBudget {
        PrefixWalkBudget {
            max_depth,
            max_entries,
            max_work,
        }
    }

    fn include(_: &Path, _: &PinnedDirectoryEntryMetadata) -> PrefixWalkDecision {
        PrefixWalkDecision::Include
    }

    fn paths(walk: &PrefixWalk) -> Vec<String> {
        walk.entries
            .iter()
            .map(|entry| entry.relative_path.to_string_lossy().into_owned())
            .collect()
    }

    fn record<'a>(walk: &'a PrefixWalk, relative: &str) -> &'a PrefixWalkDirectory {
        walk.directories
            .iter()
            .find(|directory| directory.relative_path == Path::new(relative))
            .unwrap_or_else(|| panic!("no coverage record for {relative:?}"))
    }

    #[test]
    fn complete_walk_reports_every_entry_in_name_order_with_sizes() {
        let temporary = tempfile::tempdir().expect("temporary root");
        std::fs::create_dir(temporary.path().join("b")).expect("create b");
        std::fs::write(temporary.path().join("b/inner"), b"12345").expect("write inner");
        std::fs::write(temporary.path().join("a"), b"xy").expect("write a");
        std::fs::write(temporary.path().join("c"), b"").expect("write c");

        let walk = pinned(temporary.path())
            .walk_prefix_no_follow(budget(4, 100, 1_000), include)
            .expect("walk");

        assert_eq!(paths(&walk), ["a", "b", "b/inner", "c"]);
        assert_eq!(walk.entries[0].size, Some(2));
        assert_eq!(walk.entries[1].size, None);
        assert_eq!(walk.entries[2].size, Some(5));
        assert_eq!(walk.entries[2].depth, 1);
        assert_eq!(walk.exhausted, None);
        assert!(walk.is_complete());
        assert_eq!(walk.directories.len(), 2);
        assert_eq!(record(&walk, "b").coverage, PrefixDirectoryCoverage::Complete);
        // Two directory opens, four names read and four inspections.
        assert_eq!(walk.work_used, 10);
    }

    #[test]
    fn entry_budget_met_exactly_is_not_exhaustion() {
        let temporary = tempfile::tempdir().expect("temporary root");
        std::fs::write(temporary.path().join("a"), b"").expect("write a");
        std::fs::write(temporary.path().join("b"), b"").expect("write b");

        let walk = pinned(temporary.path())
            .walk_prefix_no_follow(budget(1, 2, 1_000), include)
            .expect("walk");

        assert_eq!(paths(&walk), ["a", "b"]);
        assert_eq!(walk.exhausted, None);
        assert!(walk.is_complete());
    }

    #[test]
    fn entry_budget_exhaustion_reports_a_name_prefix_after_work_ran() {
        let temporary = tempfile::tempdir().expect("temporary root");
        std::fs::create_dir(temporary.path().join("a")).expect("create a");
        for name in ["one", "two", "three"] {
            std::fs::write(temporary.path().join("a").join(name), b"").expect("write child");
        }
        std::fs::write(temporary.path().join("z"), b"").expect("write z");

        let walk = pinned(temporary.path())
            .walk_prefix_no_follow(budget(4, 3, 1_000), include)
            .expect("walk");

        // The directory, then its first two children in name order. Nothing
        // is marked truncated before the fourth included entry is refused.
        assert_eq!(paths(&walk), ["a", "a/one", "a/three"]);
        assert_eq!(walk.exhausted, Some(PrefixWalkExhaustion::Entries));
        assert!(!walk.is_complete());
        assert_eq!(walk.root().coverage, PrefixDirectoryCoverage::NamePrefix);
        assert_eq!(walk.root().names_read, 2);
        assert_eq!(walk.root().reported, 1);
        let inner = record(&walk, "a");
        assert_eq!(inner.coverage, PrefixDirectoryCoverage::NamePrefix);
        assert_eq!(inner.names_read, 3);
        assert_eq!(inner.reported, 2);
    }

    #[test]
    fn nested_walk_continues_after_a_large_subdirectory() {
        let temporary = tempfile::tempdir().expect("temporary root");
        std::fs::create_dir(temporary.path().join("a")).expect("create a");
        for index in 0..5 {
            std::fs::write(temporary.path().join("a").join(format!("f{index}")), b"")
                .expect("write child");
        }
        std::fs::write(temporary.path().join("b"), b"").expect("write b");
        std::fs::write(temporary.path().join("c"), b"").expect("write c");

        let walk = pinned(temporary.path())
            .walk_prefix_no_follow(budget(4, 8, 1_000), include)
            .expect("walk");

        assert_eq!(
            paths(&walk),
            ["a", "a/f0", "a/f1", "a/f2", "a/f3", "a/f4", "b", "c"]
        );
        assert!(walk.is_complete());
    }

    #[test]
    fn work_budget_cuts_the_stream_and_reports_a_stream_prefix() {
        let temporary = tempfile::tempdir().expect("temporary root");
        for index in 0..10 {
            std::fs::write(temporary.path().join(format!("f{index}")), b"").expect("write");
        }

        let walk = pinned(temporary.path())
            .walk_prefix_no_follow(budget(1, 100, 5), include)
            .expect("walk");

        // One open leaves four units: two names read, two inspected.
        assert_eq!(walk.entries.len(), 2);
        assert_eq!(walk.root().names_read, 2);
        assert_eq!(walk.root().coverage, PrefixDirectoryCoverage::StreamPrefix);
        assert_eq!(walk.exhausted, Some(PrefixWalkExhaustion::Work));
        assert_eq!(walk.work_used, 5);
        assert!(!walk.is_complete());
    }

    #[test]
    fn stream_allowance_reaching_the_end_exactly_is_complete() {
        let temporary = tempfile::tempdir().expect("temporary root");
        std::fs::write(temporary.path().join("a"), b"").expect("write a");
        std::fs::write(temporary.path().join("b"), b"").expect("write b");

        // One open, two names and two inspections fit five units exactly.
        let walk = pinned(temporary.path())
            .walk_prefix_no_follow(budget(1, 100, 5), include)
            .expect("walk");

        assert_eq!(paths(&walk), ["a", "b"]);
        assert_eq!(walk.exhausted, None);
        assert!(walk.is_complete());
    }

    #[test]
    fn work_budget_exhausted_before_a_child_open_leaves_it_not_visited() {
        let temporary = tempfile::tempdir().expect("temporary root");
        std::fs::create_dir(temporary.path().join("d")).expect("create d");
        std::fs::write(temporary.path().join("d/inner"), b"").expect("write inner");

        // Open, one name, one inspection: nothing is left to open `d`.
        let walk = pinned(temporary.path())
            .walk_prefix_no_follow(budget(4, 100, 3), include)
            .expect("walk");

        assert_eq!(paths(&walk), ["d"]);
        assert_eq!(walk.exhausted, Some(PrefixWalkExhaustion::Work));
        assert_eq!(walk.root().coverage, PrefixDirectoryCoverage::Complete);
        assert_eq!(record(&walk, "d").coverage, PrefixDirectoryCoverage::NotVisited);
        assert!(!walk.is_complete());
    }

    #[test]
    fn zero_budgets_report_honest_coverage() {
        let temporary = tempfile::tempdir().expect("temporary root");
        std::fs::write(temporary.path().join("a"), b"").expect("write a");
        let root = pinned(temporary.path());

        let no_work = root
            .walk_prefix_no_follow(budget(1, 100, 0), include)
            .expect("walk without work");
        assert!(no_work.entries.is_empty());
        assert_eq!(no_work.root().coverage, PrefixDirectoryCoverage::NotVisited);
        assert_eq!(no_work.exhausted, Some(PrefixWalkExhaustion::Work));

        let no_depth = root
            .walk_prefix_no_follow(budget(0, 100, 100), include)
            .expect("walk without depth");
        assert_eq!(no_depth.root().coverage, PrefixDirectoryCoverage::DepthLimit);
        assert_eq!(no_depth.exhausted, None);
        assert!(!no_depth.is_complete());

        let no_entries = root
            .walk_prefix_no_follow(budget(1, 0, 100), include)
            .expect("walk without entries");
        assert!(no_entries.entries.is_empty());
        assert_eq!(no_entries.exhausted, Some(PrefixWalkExhaustion::Entries));

        assert!(
            root.walk_prefix_no_follow(budget(MAX_PREFIX_WALK_DEPTH + 1, 1, 1), include)
                .is_err()
        );
        assert!(
            root.walk_prefix_no_follow(budget(1, 1, MAX_PREFIX_WALK_WORK + 1), include)
                .is_err()
        );
        assert!(
            root.walk_prefix_no_follow(budget(1, 100, MAX_PREFIX_WALK_WORK), include)
                .is_ok()
        );
    }

    #[test]
    fn depth_limit_marks_unopened_directories() {
        let temporary = tempfile::tempdir().expect("temporary root");
        std::fs::create_dir_all(temporary.path().join("d/e/f")).expect("create tree");

        let walk = pinned(temporary.path())
            .walk_prefix_no_follow(budget(2, 100, 1_000), include)
            .expect("walk");

        assert_eq!(paths(&walk), ["d", "d/e"]);
        assert_eq!(walk.exhausted, None);
        assert_eq!(record(&walk, "d").coverage, PrefixDirectoryCoverage::Complete);
        assert_eq!(record(&walk, "d/e").coverage, PrefixDirectoryCoverage::DepthLimit);
        assert_eq!(record(&walk, "d/e").depth, 2);
        assert!(!walk.is_complete());
    }

    #[test]
    fn excluded_entries_are_not_reported_descended_or_charged_to_entries() {
        let temporary = tempfile::tempdir().expect("temporary root");
        std::fs::create_dir(temporary.path().join("skip")).expect("create skip");
        std::fs::write(temporary.path().join("skip/hidden"), b"").expect("write hidden");
        std::fs::write(temporary.path().join("keep"), b"").expect("write keep");

        let walk = pinned(temporary.path())
            .walk_prefix_no_follow(budget(4, 1, 1_000), |path, _| {
                if path == Path::new("skip") {
                    PrefixWalkDecision::Exclude
                } else {
                    PrefixWalkDecision::Include
                }
            })
            .expect("walk");

        assert_eq!(paths(&walk), ["keep"]);
        assert_eq!(walk.root().excluded, 1);
        assert_eq!(walk.directories.len(), 1);
        assert!(walk.is_complete());
    }

    #[test]
    fn mounted_directory_is_reported_but_never_entered() {
        let root = PinnedDirectory::open(Path::new("/"))
            .expect("open the filesystem root")
            .expect("the filesystem root exists");
        assert!(
            Path::new("/proc/self").exists(),
            "/proc is mounted on every Linux host this test runs on"
        );
        let walk = root
            .walk_prefix_no_follow(budget(3, 100, 10_000), |relative, _| {
                if relative == Path::new("proc") {
                    PrefixWalkDecision::Include
                } else {
                    PrefixWalkDecision::Exclude
                }
            })
            .expect("walk");
        assert_eq!(paths(&walk), ["proc"]);
        assert_eq!(
            record(&walk, "proc").coverage,
            PrefixDirectoryCoverage::MountBoundary
        );
        assert!(!walk.is_complete());
    }

    #[test]
    fn symlinks_are_reported_without_being_followed() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let outside = tempfile::tempdir().expect("outside root");
        std::fs::write(outside.path().join("secret"), b"").expect("write secret");
        std::os::unix::fs::symlink(outside.path(), temporary.path().join("link"))
            .expect("create link");

        let walk = pinned(temporary.path())
            .walk_prefix_no_follow(budget(4, 100, 1_000), include)
            .expect("walk");

        assert_eq!(paths(&walk), ["link"]);
        assert_eq!(walk.entries[0].metadata.entry_type, PinnedEntryType::Symlink);
        assert_eq!(walk.entries[0].size, None);
        assert_eq!(walk.directories.len(), 1);
        assert!(walk.is_complete());
    }

    #[test]
    fn entry_removed_mid_walk_is_recorded_as_vanished() {
        let temporary = tempfile::tempdir().expect("temporary root");
        for name in ["a", "b", "c"] {
            std::fs::write(temporary.path().join(name), b"").expect("write");
        }
        let root_path = temporary.path().to_path_buf();

        let walk = pinned(temporary.path())
            .walk_prefix_no_follow(budget(1, 100, 1_000), |path, _| {
                if path == Path::new("a") {
                    std::fs::remove_file(root_path.join("b")).expect("remove b mid-walk");
                }
                PrefixWalkDecision::Include
            })
            .expect("walk");

        assert_eq!(paths(&walk), ["a", "c"]);
        assert_eq!(walk.root().vanished, 1);
        assert!(walk.root().changed);
        assert_eq!(walk.root().coverage, PrefixDirectoryCoverage::Complete);
        assert!(!walk.root().is_complete());
        assert!(!walk.is_complete());
    }

    #[test]
    fn directory_replaced_mid_walk_is_not_descended() {
        let temporary = tempfile::tempdir().expect("temporary root");
        std::fs::create_dir(temporary.path().join("d")).expect("create d");
        std::fs::write(temporary.path().join("d/original"), b"").expect("write original");
        let root_path = temporary.path().to_path_buf();

        let walk = pinned(temporary.path())
            .walk_prefix_no_follow(budget(4, 100, 1_000), |path, _| {
                if path == Path::new("d") {
                    std::fs::rename(root_path.join("d"), root_path.join("moved"))
                        .expect("move d mid-walk");
                    std::fs::create_dir(root_path.join("d")).expect("replace d");
                    std::fs::write(root_path.join("d/substitute"), b"").expect("write substitute");
                }
                PrefixWalkDecision::Include
            })
            .expect("walk");

        assert_eq!(paths(&walk), ["d"]);
        assert_eq!(record(&walk, "d").coverage, PrefixDirectoryCoverage::Replaced);
        assert!(walk.root().changed);
        assert!(!walk.is_complete());
    }

    #[test]
    fn unreadable_child_directory_is_recorded_as_failed_and_the_walk_continues() {
        // SAFETY: geteuid has no preconditions.
        if unsafe { libc::geteuid() } == 0 {
            // Permission bits do not restrict the superuser; the error
            // classification itself is covered without a filesystem.
            eprintln!(
                "skipping unreadable_child_directory_is_recorded_as_failed_and_the_walk_continues: \
                 running as root"
            );
            return;
        }
        use std::os::unix::fs::PermissionsExt as _;
        let temporary = tempfile::tempdir().expect("temporary root");
        let locked = temporary.path().join("locked");
        std::fs::create_dir(&locked).expect("create locked");
        std::fs::write(locked.join("inner"), b"").expect("write inner");
        std::fs::write(temporary.path().join("z"), b"").expect("write z");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000))
            .expect("lock directory");

        let walk = pinned(temporary.path()).walk_prefix_no_follow(budget(4, 100, 1_000), include);
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700))
            .expect("unlock directory");
        let walk = walk.expect("walk");

        assert_eq!(paths(&walk), ["locked", "z"]);
        let failed = record(&walk, "locked");
        assert_eq!(failed.coverage, PrefixDirectoryCoverage::Failed);
        assert!(failed.failure.is_some());
        assert!(walk.root().is_complete());
        assert!(!walk.is_complete());
    }

    #[test]
    fn open_errors_are_classified_without_a_filesystem() {
        for errno in [libc::ENOENT, libc::ENOTDIR, libc::ELOOP] {
            assert!(
                matches!(
                    child_open_failure(std::io::Error::from_raw_os_error(errno)),
                    ChildOpen::Replaced
                ),
                "errno {errno} must classify as replaced"
            );
        }
        for errno in [libc::EACCES, libc::EIO, libc::EMFILE] {
            assert!(
                matches!(
                    child_open_failure(std::io::Error::from_raw_os_error(errno)),
                    ChildOpen::Failed(_)
                ),
                "errno {errno} must classify as failed"
            );
        }
    }

    #[test]
    fn directory_read_failure_is_a_failed_frame_not_a_walk_error() {
        let temporary = tempfile::tempdir().expect("temporary root");
        // A regular-file descriptor cannot be reopened as a directory, so
        // reading its names fails the way a vanished directory inode would.
        let file_path = temporary.path().join("plain");
        std::fs::write(&file_path, b"").expect("write plain");
        let file = File::open(&file_path).expect("open plain");
        let mut work = WorkMeter { used: 0, max: 100 };
        let mut walk = PrefixWalk {
            entries: Vec::new(),
            directories: vec![PrefixWalkDirectory::unvisited(PathBuf::new(), 0)],
            exhausted: None,
            work_used: 0,
        };

        let read = read_frame(file, PathBuf::new(), 0, 0, &mut work, &mut walk)
            .expect("a read failure is not a walk error");

        assert!(matches!(read, FrameRead::Failed(_)));
        assert_eq!(work.used, 0);
    }
}
