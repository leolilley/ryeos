use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Condvar, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::error::EngineError;
use crate::parsers::ParserDispatcher;

const MAX_CACHE_ENTRIES: usize = 64;
const MAX_CACHE_BYTES: usize = 8 * 1024 * 1024;
const MAX_IN_FLIGHT_BUILDS: usize = 64;
const VERIFY_HITS_ENV: &str = "RYEOS_PARSER_OVERLAY_CACHE_VERIFY_HITS";

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct ParserOverlayCacheKey {
    pub project_root: PathBuf,
    pub overlay_fingerprint: String,
    pub effective_trust_fingerprint: String,
    pub base_trust_fingerprint: String,
    pub caller_overlay_identity: Option<String>,
    pub generation_fingerprint: String,
}

#[derive(Debug)]
struct CacheEntry {
    dispatcher: ParserDispatcher,
    estimated_bytes: usize,
}

#[derive(Debug, Default)]
struct CacheState {
    entries: HashMap<ParserOverlayCacheKey, CacheEntry>,
    lru: VecDeque<ParserOverlayCacheKey>,
    latest_by_project: HashMap<PathBuf, ParserOverlayCacheKey>,
    in_flight: HashMap<ParserOverlayCacheKey, std::sync::Arc<InFlight>>,
    total_bytes: usize,
}

#[derive(Debug, Default)]
struct InFlight {
    result: Mutex<Option<Result<ParserDispatcher, String>>>,
    completed: Condvar,
}

/// A deliberately small, engine-generation-local parser overlay cache.
#[derive(Debug, Default)]
pub(crate) struct ParserOverlayCache {
    state: Mutex<CacheState>,
}

impl ParserOverlayCache {
    pub(crate) fn get_or_build(
        &self,
        key: ParserOverlayCacheKey,
        cacheable: bool,
        overlay_bytes: u64,
        build: impl FnOnce() -> Result<ParserDispatcher, EngineError>,
    ) -> Result<ParserDispatcher, EngineError> {
        let mut state = self.state.lock().map_err(|_| {
            EngineError::Internal("parser overlay cache mutex was poisoned".to_string())
        })?;

        if cacheable {
            if let Some(dispatcher) = state
                .entries
                .get(&key)
                .map(|entry| entry.dispatcher.clone())
            {
                touch_lru(&mut state.lru, &key);
                if verify_hits_enabled() {
                    drop(state);
                    let rebuilt = build()?;
                    if rebuilt.parser_tools.fingerprint() != dispatcher.parser_tools.fingerprint()
                        || rebuilt.handler_cache_identity() != dispatcher.handler_cache_identity()
                    {
                        return Err(EngineError::Internal(format!(
                            "parser overlay cache verification diverged for {}",
                            key.project_root.display()
                        )));
                    }
                    tracing::info!(
                        project_root = %key.project_root.display(),
                        overlay_fingerprint = %key.overlay_fingerprint,
                        "parser overlay cache cold/hot verification passed"
                    );
                    return Ok(dispatcher);
                }
                tracing::debug!(
                    project_root = %key.project_root.display(),
                    overlay_fingerprint = %key.overlay_fingerprint,
                    "parser overlay cache hit"
                );
                return Ok(dispatcher);
            }
        }

        // A same-granule fingerprint is deliberately non-cacheable. It also
        // cannot join an in-flight build: an edit may have happened after
        // that builder captured its input while retaining the same coarse
        // metadata tuple.
        if !cacheable {
            tracing::debug!(
                project_root = %key.project_root.display(),
                rebuild_reason = "same_granule_metadata",
                "bypassing parser overlay cache and single-flight sharing"
            );
            drop(state);
            return build();
        }

        if let Some(in_flight) = state.in_flight.get(&key).cloned() {
            tracing::debug!(
                project_root = %key.project_root.display(),
                "waiting for in-flight parser overlay rebuild"
            );
            drop(state);
            let mut result = in_flight.result.lock().map_err(|_| {
                EngineError::Internal("parser overlay single-flight mutex was poisoned".to_string())
            })?;
            while result.is_none() {
                result = in_flight.completed.wait(result).map_err(|_| {
                    EngineError::Internal(
                        "parser overlay single-flight wait was poisoned".to_string(),
                    )
                })?;
            }
            return match result.as_ref().expect("result checked above") {
                Ok(dispatcher) => Ok(dispatcher.clone()),
                Err(reason) => Err(EngineError::Internal(format!(
                    "in-flight parser overlay rebuild failed: {reason}"
                ))),
            };
        }

        if state.in_flight.len() >= MAX_IN_FLIGHT_BUILDS {
            tracing::debug!(
                project_root = %key.project_root.display(),
                in_flight = state.in_flight.len(),
                in_flight_limit = MAX_IN_FLIGHT_BUILDS,
                "parser overlay cache bypassed because the single-flight table is full"
            );
            drop(state);
            return build();
        }

        let rebuild_reason = rebuild_reason(state.latest_by_project.get(&key.project_root), &key);
        tracing::debug!(
            project_root = %key.project_root.display(),
            rebuild_reason,
            "rebuilding effective parser overlay"
        );
        let in_flight = std::sync::Arc::new(InFlight::default());
        state.in_flight.insert(key.clone(), in_flight.clone());
        drop(state);

        let build_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(build));
        let build_result = match build_result {
            Ok(result) => result,
            Err(panic_payload) => {
                let mut state = self
                    .state
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                state.in_flight.remove(&key);
                if let Ok(mut result) = in_flight.result.lock() {
                    *result = Some(Err(
                        "parser overlay rebuild panicked before producing a result".to_string(),
                    ));
                    in_flight.completed.notify_all();
                }
                drop(state);
                std::panic::resume_unwind(panic_payload);
            }
        };
        let mut state = self.state.lock().map_err(|_| {
            EngineError::Internal("parser overlay cache mutex was poisoned".to_string())
        })?;
        state.in_flight.remove(&key);

        if let Ok(dispatcher) = &build_result {
            if cacheable {
                let estimated_bytes = usize::try_from(overlay_bytes)
                    .unwrap_or(usize::MAX)
                    .saturating_add(dispatcher.parser_tools.estimated_size_bytes())
                    .saturating_add(estimated_key_bytes(&key));
                if estimated_bytes > MAX_CACHE_BYTES {
                    tracing::debug!(
                        project_root = %key.project_root.display(),
                        estimated_bytes,
                        cache_limit_bytes = MAX_CACHE_BYTES,
                        "effective parser overlay exceeds cache byte limit"
                    );
                } else {
                    insert_entry(&mut state, &key, dispatcher, estimated_bytes);
                }
            }
        }

        let shared_result = build_result
            .as_ref()
            .map(|dispatcher| dispatcher.clone())
            .map_err(ToString::to_string);
        if let Ok(mut result) = in_flight.result.lock() {
            *result = Some(shared_result);
            in_flight.completed.notify_all();
        }
        drop(state);
        build_result
    }
}

fn verify_hits_enabled() -> bool {
    matches!(std::env::var(VERIFY_HITS_ENV).as_deref(), Ok("1"))
}

fn insert_entry(
    state: &mut CacheState,
    key: &ParserOverlayCacheKey,
    dispatcher: &ParserDispatcher,
    estimated_bytes: usize,
) {
    // A registered engine generation is immutable authority. Once this
    // project observes a successor generation, entries bound to predecessors
    // are unreachable and are retired immediately rather than waiting for LRU.
    let obsolete_generation_keys = state
        .entries
        .keys()
        .filter(|candidate| {
            candidate.project_root == key.project_root
                && candidate.generation_fingerprint != key.generation_fingerprint
        })
        .cloned()
        .collect::<Vec<_>>();
    for obsolete in obsolete_generation_keys {
        if let Some(entry) = state.entries.remove(&obsolete) {
            state.total_bytes = state.total_bytes.saturating_sub(entry.estimated_bytes);
        }
        if let Some(position) = state
            .lru
            .iter()
            .position(|candidate| candidate == &obsolete)
        {
            state.lru.remove(position);
        }
    }

    if let Some(replaced) = state.entries.remove(key) {
        state.total_bytes = state.total_bytes.saturating_sub(replaced.estimated_bytes);
        touch_lru(&mut state.lru, key);
    } else {
        state.lru.push_back(key.clone());
    }
    state.total_bytes = state.total_bytes.saturating_add(estimated_bytes);
    state.entries.insert(
        key.clone(),
        CacheEntry {
            dispatcher: dispatcher.clone(),
            estimated_bytes,
        },
    );
    state
        .latest_by_project
        .insert(key.project_root.clone(), key.clone());

    while state.entries.len() > MAX_CACHE_ENTRIES || state.total_bytes > MAX_CACHE_BYTES {
        let Some(evicted_key) = state.lru.pop_front() else {
            break;
        };
        if let Some(evicted) = state.entries.remove(&evicted_key) {
            state.total_bytes = state.total_bytes.saturating_sub(evicted.estimated_bytes);
        }
        if state.latest_by_project.get(&evicted_key.project_root) == Some(&evicted_key) {
            state.latest_by_project.remove(&evicted_key.project_root);
        }
        tracing::debug!(
            project_root = %evicted_key.project_root.display(),
            "evicted effective parser overlay from bounded cache"
        );
    }
}

fn touch_lru(lru: &mut VecDeque<ParserOverlayCacheKey>, key: &ParserOverlayCacheKey) {
    if let Some(position) = lru.iter().position(|candidate| candidate == key) {
        lru.remove(position);
    }
    lru.push_back(key.clone());
}

fn rebuild_reason(
    previous: Option<&ParserOverlayCacheKey>,
    current: &ParserOverlayCacheKey,
) -> &'static str {
    let Some(previous) = previous else {
        return "cold";
    };
    if previous.generation_fingerprint != current.generation_fingerprint {
        return "generation_changed";
    }
    if previous.base_trust_fingerprint != current.base_trust_fingerprint {
        return "base_trust_changed";
    }
    if previous.caller_overlay_identity != current.caller_overlay_identity {
        return "caller_overlay_changed";
    }
    if previous.effective_trust_fingerprint != current.effective_trust_fingerprint {
        return "project_trust_changed";
    }
    if previous.overlay_fingerprint != current.overlay_fingerprint {
        return "overlay_metadata_changed";
    }
    "evicted"
}

fn estimated_key_bytes(key: &ParserOverlayCacheKey) -> usize {
    key.project_root.as_os_str().as_encoded_bytes().len()
        + key.overlay_fingerprint.len()
        + key.effective_trust_fingerprint.len()
        + key.base_trust_fingerprint.len()
        + key
            .caller_overlay_identity
            .as_ref()
            .map(String::len)
            .unwrap_or(0)
        + key.generation_fingerprint.len()
}

#[derive(Debug)]
pub(crate) struct ParserOverlayMetadata {
    pub fingerprint: String,
    pub cacheable: bool,
    pub total_file_bytes: u64,
}

/// Fingerprint every file under the effective parser overlay root using the
/// same conservative metadata tuple as Git's stat cache. Files modified or
/// inode-changed in the current clock second are always dirty so a coarse
/// filesystem timestamp cannot produce a racy-clean cache hit.
pub(crate) fn fingerprint_parser_overlay(
    overlay_root: &Path,
) -> Result<ParserOverlayMetadata, EngineError> {
    let now_seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let mut records = Vec::new();
    let mut total_file_bytes = 0u64;
    let mut cacheable = true;
    let mut visited_directories = HashSet::new();
    walk_metadata(
        overlay_root,
        overlay_root,
        now_seconds,
        &mut visited_directories,
        &mut records,
        &mut total_file_bytes,
        &mut cacheable,
    )?;
    records.sort_by(|left, right| left.0.cmp(&right.0));

    let mut bytes = Vec::new();
    for (relative_path, metadata) in records {
        append_field(&mut bytes, relative_path.as_os_str().as_encoded_bytes());
        append_field(&mut bytes, &metadata.size.to_le_bytes());
        append_field(&mut bytes, &metadata.mtime_seconds.to_le_bytes());
        append_field(&mut bytes, &metadata.mtime_nanoseconds.to_le_bytes());
        append_field(&mut bytes, &metadata.ctime_seconds.to_le_bytes());
        append_field(&mut bytes, &metadata.ctime_nanoseconds.to_le_bytes());
        append_field(&mut bytes, &metadata.inode.to_le_bytes());
    }

    Ok(ParserOverlayMetadata {
        fingerprint: lillux::cas::sha256_hex(&bytes),
        cacheable,
        total_file_bytes,
    })
}

#[derive(Debug)]
struct FileMetadata {
    size: u64,
    mtime_seconds: i64,
    mtime_nanoseconds: i64,
    ctime_seconds: i64,
    ctime_nanoseconds: i64,
    inode: u64,
}

fn walk_metadata(
    overlay_root: &Path,
    current: &Path,
    now_seconds: u64,
    visited_directories: &mut HashSet<PathBuf>,
    records: &mut Vec<(PathBuf, FileMetadata)>,
    total_file_bytes: &mut u64,
    cacheable: &mut bool,
) -> Result<(), EngineError> {
    let canonical =
        std::fs::canonicalize(current).map_err(|error| EngineError::SchemaLoaderError {
            reason: format!(
                "cannot canonicalize parsers dir {}: {error}",
                current.display()
            ),
        })?;
    if !visited_directories.insert(canonical) {
        return Err(EngineError::SchemaLoaderError {
            reason: format!(
                "parser overlay contains a directory cycle at {}",
                current.display()
            ),
        });
    }

    let entries = std::fs::read_dir(current).map_err(|error| EngineError::SchemaLoaderError {
        reason: format!("cannot read parsers dir {}: {error}", current.display()),
    })?;
    let mut paths = entries
        .map(|entry| {
            entry
                .map(|entry| entry.path())
                .map_err(|error| EngineError::SchemaLoaderError {
                    reason: format!(
                        "cannot read an entry in parsers dir {}: {error}",
                        current.display()
                    ),
                })
        })
        .collect::<Result<Vec<_>, _>>()?;
    paths.sort();

    for path in paths {
        let metadata =
            std::fs::metadata(&path).map_err(|error| EngineError::SchemaLoaderError {
                reason: format!(
                    "cannot stat parser overlay path {}: {error}",
                    path.display()
                ),
            })?;
        if metadata.is_dir() {
            walk_metadata(
                overlay_root,
                &path,
                now_seconds,
                visited_directories,
                records,
                total_file_bytes,
                cacheable,
            )?;
            continue;
        }
        if !metadata.is_file() {
            continue;
        }

        let relative_path = path
            .strip_prefix(overlay_root)
            .map_err(|error| EngineError::SchemaLoaderError {
                reason: format!(
                    "parser overlay path {} escaped root {}: {error}",
                    path.display(),
                    overlay_root.display()
                ),
            })?
            .to_path_buf();
        let file_metadata = platform_file_metadata(&metadata);
        let current_second = i64::try_from(now_seconds).unwrap_or(i64::MAX);
        if timestamp_is_racy(
            file_metadata.mtime_seconds,
            file_metadata.mtime_nanoseconds,
            current_second,
        ) || timestamp_is_racy(
            file_metadata.ctime_seconds,
            file_metadata.ctime_nanoseconds,
            current_second,
        ) {
            *cacheable = false;
        }
        *total_file_bytes = total_file_bytes.saturating_add(file_metadata.size);
        records.push((relative_path, file_metadata));
    }

    Ok(())
}

fn timestamp_is_racy(timestamp_seconds: i64, timestamp_nanoseconds: i64, now_seconds: i64) -> bool {
    if timestamp_seconds >= now_seconds {
        return true;
    }
    // A zero sub-second field can represent a filesystem whose writable
    // timestamp granule is coarser than one second (not merely an event that
    // happened exactly on a second). Keep the preceding second dirty too,
    // covering the two-second granule used by the coarsest filesystems RyeOS
    // supports. False dirtiness only rebuilds; false cleanliness is unsafe.
    timestamp_nanoseconds == 0 && timestamp_seconds >= now_seconds.saturating_sub(1)
}

#[cfg(unix)]
fn platform_file_metadata(metadata: &std::fs::Metadata) -> FileMetadata {
    use std::os::unix::fs::MetadataExt;

    FileMetadata {
        size: metadata.len(),
        mtime_seconds: metadata.mtime(),
        mtime_nanoseconds: metadata.mtime_nsec(),
        ctime_seconds: metadata.ctime(),
        ctime_nanoseconds: metadata.ctime_nsec(),
        inode: metadata.ino(),
    }
}

#[cfg(not(unix))]
fn platform_file_metadata(metadata: &std::fs::Metadata) -> FileMetadata {
    let modified = metadata
        .modified()
        .ok()
        .and_then(|value| value.duration_since(UNIX_EPOCH).ok())
        .unwrap_or_default();
    FileMetadata {
        size: metadata.len(),
        mtime_seconds: i64::try_from(modified.as_secs()).unwrap_or(i64::MAX),
        mtime_nanoseconds: i64::from(modified.subsec_nanos()),
        ctime_seconds: i64::try_from(modified.as_secs()).unwrap_or(i64::MAX),
        ctime_nanoseconds: i64::from(modified.subsec_nanos()),
        inode: 0,
    }
}

fn append_field(bytes: &mut Vec<u8>, field: &[u8]) {
    bytes.extend_from_slice(&(field.len() as u64).to_le_bytes());
    bytes.extend_from_slice(field);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn key(project: &str) -> ParserOverlayCacheKey {
        ParserOverlayCacheKey {
            project_root: PathBuf::from(project),
            overlay_fingerprint: "overlay".to_string(),
            effective_trust_fingerprint: "effective-trust".to_string(),
            base_trust_fingerprint: "base-trust".to_string(),
            caller_overlay_identity: None,
            generation_fingerprint: "generation".to_string(),
        }
    }

    #[test]
    fn concurrent_identical_lookups_build_once() {
        let cache = std::sync::Arc::new(ParserOverlayCache::default());
        let builds = std::sync::Arc::new(AtomicUsize::new(0));
        let build_started = std::sync::Arc::new(std::sync::Barrier::new(2));
        let release_build = std::sync::Arc::new(std::sync::Barrier::new(2));
        std::thread::scope(|scope| {
            for _ in 0..8 {
                let cache = std::sync::Arc::clone(&cache);
                let builds = std::sync::Arc::clone(&builds);
                let build_started = std::sync::Arc::clone(&build_started);
                let release_build = std::sync::Arc::clone(&release_build);
                scope.spawn(move || {
                    cache
                        .get_or_build(key("/project"), true, 0, || {
                            builds.fetch_add(1, Ordering::SeqCst);
                            build_started.wait();
                            release_build.wait();
                            Ok(
                                crate::parsers::test_helpers::
                                    dispatcher_with_canonical_bundle_descriptors(),
                            )
                        })
                        .unwrap();
                });
            }
            build_started.wait();
            release_build.wait();
        });
        assert_eq!(builds.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn cache_is_entry_bounded() {
        let cache = ParserOverlayCache::default();
        for index in 0..(MAX_CACHE_ENTRIES + 4) {
            cache
                .get_or_build(key(&format!("/project/{index}")), true, 0, || {
                    Ok(
                        crate::parsers::test_helpers::dispatcher_with_canonical_bundle_descriptors(
                        ),
                    )
                })
                .unwrap();
        }
        let state = cache.state.lock().unwrap();
        assert!(state.entries.len() <= MAX_CACHE_ENTRIES);
        assert!(state.total_bytes <= MAX_CACHE_BYTES);
        assert!(state.latest_by_project.len() <= MAX_CACHE_ENTRIES);
    }

    #[test]
    fn successor_generation_retires_predecessor_entries_immediately() {
        let cache = ParserOverlayCache::default();
        let mut predecessor = key("/project");
        predecessor.generation_fingerprint = "generation-one".to_string();
        let mut successor = predecessor.clone();
        successor.generation_fingerprint = "generation-two".to_string();
        for cache_key in [predecessor.clone(), successor.clone()] {
            cache
                .get_or_build(cache_key, true, 0, || {
                    Ok(
                        crate::parsers::test_helpers::dispatcher_with_canonical_bundle_descriptors(
                        ),
                    )
                })
                .unwrap();
        }

        let state = cache.state.lock().unwrap();
        assert!(!state.entries.contains_key(&predecessor));
        assert!(state.entries.contains_key(&successor));
    }

    #[test]
    fn same_granule_lookup_never_reuses_cache_or_single_flight() {
        let cache = ParserOverlayCache::default();
        let builds = AtomicUsize::new(0);
        for _ in 0..2 {
            cache
                .get_or_build(key("/dirty-project"), false, 0, || {
                    builds.fetch_add(1, Ordering::SeqCst);
                    Ok(
                        crate::parsers::test_helpers::dispatcher_with_canonical_bundle_descriptors(
                        ),
                    )
                })
                .unwrap();
        }
        assert_eq!(builds.load(Ordering::SeqCst), 2);
        assert!(cache.state.lock().unwrap().entries.is_empty());
    }

    #[test]
    fn zero_subsecond_timestamp_keeps_preceding_second_dirty() {
        assert!(timestamp_is_racy(99, 0, 100));
        assert!(!timestamp_is_racy(98, 0, 100));
        assert!(!timestamp_is_racy(99, 1, 100));
        assert!(timestamp_is_racy(100, 1, 100));
    }

    #[test]
    fn recursive_metadata_fingerprint_includes_non_descriptor_files() {
        let root = tempfile::tempdir().unwrap();
        let nested = root.path().join("nested");
        std::fs::create_dir_all(&nested).unwrap();
        let file = nested.join("notes.txt");
        std::fs::write(&file, "one").unwrap();
        let first = fingerprint_parser_overlay(root.path()).unwrap();

        std::fs::write(&file, "a longer non-descriptor file").unwrap();
        let second = fingerprint_parser_overlay(root.path()).unwrap();

        assert_ne!(first.fingerprint, second.fingerprint);
        assert_ne!(first.total_file_bytes, second.total_file_bytes);
    }
}
