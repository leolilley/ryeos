//! Admission-time resolution cache.
//!
//! `run_resolution_pipeline` (parse + signature-verify walk + compose over the
//! whole extends/references chain) is a pure function of signed/verified
//! content plus the immutable daemon generation, yet it runs on every launch.
//! This cache stores its output keyed on `(generation, ref, project-root,
//! plan-context)` and serves a hit only after proving the outcome is still
//! current — cheaply, from content, never by recompute.
//!
//! ## Soundness
//!
//! A resolution outcome depends on both POSITIVE dependencies (the winner and
//! its ancestors/references, each carrying a whole-file digest) and NEGATIVE
//! dependencies (paths probed and found absent at a precedence at least as high
//! as the winner — recorded as [`ProbedAbsence`]). A digest-only cache is
//! unsound: if a project item APPEARS where the winner was resolved from a
//! bundle, every stored digest still matches but the correct chain is now
//! different. Revalidation therefore checks both directions.
//!
//! ## Two tiers
//!
//! - **Bundle-space** positives and absences need no revalidation: bundle
//!   content and layout are immutable for a generation's lifetime, and the
//!   generation is in the key — a `bundle.install`/`uninstall` bump makes every
//!   bundle-derived entry unreachable. A chain touching only bundle space is a
//!   pure key lookup.
//! - **Project-space** positives are re-hashed (whole-file digest compare) and
//!   project-space absences are re-probed (must still be absent). This is a
//!   handful of small file reads versus the full parse/verify/compose recompute.
//!
//! The verifier (`Engine::verify` of the admitted subject) is deliberately NOT
//! cached: it runs on every launch regardless. This cache covers the pipeline.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use ryeos_engine::contracts::{ItemSpace, ProbedAbsence};
use ryeos_engine::resolution::ResolutionOutput;

/// Identity of one admission's resolution inputs. A hit requires an exact key
/// match AND passing content-derived revalidation.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct ResolutionCacheKey {
    /// System-install generation — mixed in so any bundle install/uninstall
    /// bump makes every bundle-derived entry unreachable.
    pub generation: u64,
    /// Canonical ref being admitted.
    pub canonical_ref: String,
    /// Project-root identity that steers project-space resolution. `None` for
    /// projectless (bundle-only) resolution.
    pub project_root: Option<PathBuf>,
    /// Identity of the remaining resolution inputs the generation and project
    /// root do NOT capture, pre-rendered by the caller to one stable string:
    /// the engine/bundle generation identity, the PROJECT parser-overlay
    /// fingerprint (`.ai/parsers/`), and the effective trust identity
    /// (`.ai/trust-keys/`). An edit to any of these changes this string, so a
    /// stale resolution misses rather than being served.
    pub plan_context_identity: String,
}

struct Entry {
    output: ResolutionOutput,
    probed_absent: Vec<ProbedAbsence>,
    /// Insertion order, for bounded eviction.
    seq: u64,
}

struct Inner {
    slots: HashMap<ResolutionCacheKey, Entry>,
    next_seq: u64,
}

/// Bounded, content-revalidating store of resolved-pipeline outputs.
pub struct ResolutionCache {
    inner: Mutex<Inner>,
    capacity: usize,
}

/// Outcome of a lookup, for timing/observability at the call site.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LookupOutcome {
    /// No entry for the key.
    Miss,
    /// Entry present but revalidation failed; it was evicted.
    Stale,
    /// Entry present and revalidated fresh.
    Hit,
}

impl ResolutionCache {
    /// Create a cache holding at most `capacity` entries (a node resolves few
    /// distinct roots; tens of entries is ample).
    pub fn new(capacity: usize) -> Self {
        Self {
            inner: Mutex::new(Inner {
                slots: HashMap::new(),
                next_seq: 0,
            }),
            capacity: capacity.max(1),
        }
    }

    /// Look up `key`, revalidating any entry against current on-disk content.
    /// Returns the cached output only when it is proven still current; a stale
    /// entry is evicted and reported as [`LookupOutcome::Stale`] so the caller
    /// recomputes and re-inserts.
    pub fn get(&self, key: &ResolutionCacheKey) -> (Option<ResolutionOutput>, LookupOutcome) {
        // Snapshot the small revalidation inputs (paths + digests) under the
        // lock, then do the filesystem I/O UNLOCKED so concurrent lookups do
        // not serialize behind disk. Re-lock only to clone the (large) output
        // on a confirmed-fresh entry or to evict a stale one.
        let inputs = {
            let guard = self.inner.lock().expect("resolution cache mutex poisoned");
            match guard.slots.get(key) {
                Some(entry) => revalidation_inputs(&entry.output, &entry.probed_absent),
                None => return (None, LookupOutcome::Miss),
            }
        };
        if revalidate(&inputs) {
            let guard = self.inner.lock().expect("resolution cache mutex poisoned");
            match guard.slots.get(key) {
                // A concurrent insert may have replaced the entry between the
                // snapshot and now; its output is at least as fresh, so serving
                // it is sound.
                Some(entry) => (Some(entry.output.clone()), LookupOutcome::Hit),
                None => (None, LookupOutcome::Miss),
            }
        } else {
            let mut guard = self.inner.lock().expect("resolution cache mutex poisoned");
            guard.slots.remove(key);
            (None, LookupOutcome::Stale)
        }
    }

    /// Store a freshly computed resolution for `key`. Evicts the oldest entry
    /// when at capacity (unless replacing an existing key).
    pub fn insert(
        &self,
        key: ResolutionCacheKey,
        output: ResolutionOutput,
        probed_absent: Vec<ProbedAbsence>,
    ) {
        let mut guard = self.inner.lock().expect("resolution cache mutex poisoned");
        let seq = guard.next_seq;
        guard.next_seq += 1;
        if guard.slots.len() >= self.capacity && !guard.slots.contains_key(&key) {
            if let Some(oldest) = guard
                .slots
                .iter()
                .min_by_key(|(_, entry)| entry.seq)
                .map(|(k, _)| k.clone())
            {
                guard.slots.remove(&oldest);
            }
        }
        guard.slots.insert(
            key,
            Entry {
                output,
                probed_absent,
                seq,
            },
        );
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.inner.lock().unwrap().slots.len()
    }
}

/// The project-space revalidation inputs of a cached entry — small enough to
/// clone out from under the cache lock so the disk I/O runs unlocked. Bundle
/// dependencies are omitted: they are immutable within the generation (in the
/// key), so they are never re-read.
struct RevalidationInputs {
    /// `(source_path, whole-file digest)` for each project-space positive dep.
    positives: Vec<(PathBuf, String)>,
    /// Recorded project-space absences that must still be absent.
    absences: Vec<PathBuf>,
}

fn revalidation_inputs(
    output: &ResolutionOutput,
    probed_absent: &[ProbedAbsence],
) -> RevalidationInputs {
    let positives = std::iter::once(&output.root)
        .chain(output.ancestors.iter())
        .chain(output.referenced_items.iter())
        .filter(|dependency| dependency.source_space == ItemSpace::Project)
        .map(|dependency| {
            (
                dependency.source_path.clone(),
                dependency.source_content_digest.clone(),
            )
        })
        .collect();
    let absences = probed_absent
        .iter()
        .filter(|absence| absence.space == ItemSpace::Project)
        .map(|absence| absence.path.clone())
        .collect();
    RevalidationInputs {
        positives,
        absences,
    }
}

/// Prove a cached resolution is still current, from content only. Runs UNLOCKED
/// on the snapshotted inputs. Project positives must still hash to their
/// recorded whole-file digest; project absences must still be absent. Any
/// deviation — a changed, removed, or unreadable positive, or an appeared
/// shadow — fails closed.
fn revalidate(inputs: &RevalidationInputs) -> bool {
    for (source_path, digest) in &inputs.positives {
        match std::fs::read_to_string(source_path) {
            Ok(content) if &lillux::signature::content_hash(&content) == digest => {}
            _ => return false,
        }
    }
    for path in &inputs.absences {
        // Match the resolver's own probe. A regular file at the path is an
        // item that would now shadow the cached winner → stale. A directory or
        // dangling symlink is not an item. A non-NotFound error (e.g. EACCES)
        // is where a fresh resolve would hard-fail, so fail closed rather than
        // read it as "still absent".
        match std::fs::metadata(path) {
            Ok(metadata) if metadata.is_file() => return false,
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return false,
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_engine::resolution::{
        KindComposedView, ResolutionOutput, ResolutionStepName, ResolvedAncestor, TrustClass,
    };
    use std::path::Path;

    fn tempdir() -> PathBuf {
        use std::time::SystemTime;
        let nanos = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos() as u64;
        let dir = std::env::temp_dir().join(format!(
            "rye_reso_cache_test_{}_{}",
            std::process::id(),
            nanos
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write(path: &Path, content: &str) -> String {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, content).unwrap();
        lillux::signature::content_hash(content)
    }

    fn ancestor(space: ItemSpace, source_path: PathBuf, digest: String) -> ResolvedAncestor {
        ResolvedAncestor {
            requested_id: "x".into(),
            resolved_ref: "tool:x".into(),
            source_path,
            source_space: space,
            trust_class: match space {
                ItemSpace::Project => TrustClass::TrustedProject,
                ItemSpace::Bundle => TrustClass::TrustedBundle,
            },
            alias_resolution: None,
            added_by: ResolutionStepName::PipelineInit,
            raw_content: String::new(),
            source_content_digest: digest,
            raw_content_digest: String::new(),
        }
    }

    fn output_with_root(root: ResolvedAncestor) -> ResolutionOutput {
        ResolutionOutput {
            root,
            ancestors: Vec::new(),
            references_edges: Vec::new(),
            referenced_items: Vec::new(),
            step_outputs: Default::default(),
            effective_trust_class: TrustClass::TrustedProject,
            composed: KindComposedView::identity(serde_json::json!({})),
        }
    }

    fn key() -> ResolutionCacheKey {
        ResolutionCacheKey {
            generation: 1,
            canonical_ref: "tool:x".into(),
            project_root: Some(PathBuf::from("/p")),
            plan_context_identity: String::new(),
        }
    }

    #[test]
    fn project_positive_dependency_change_invalidates() {
        let dir = tempdir();
        let path = dir.join("item.py");
        let digest = write(&path, "# original");
        let cache = ResolutionCache::new(8);
        cache.insert(
            key(),
            output_with_root(ancestor(ItemSpace::Project, path.clone(), digest)),
            Vec::new(),
        );

        // Unchanged → hit.
        let (hit, outcome) = cache.get(&key());
        assert!(hit.is_some());
        assert_eq!(outcome, LookupOutcome::Hit);

        // A single-byte change to the project source → stale, evicted.
        write(&path, "# changed");
        let (miss, outcome) = cache.get(&key());
        assert!(miss.is_none());
        assert_eq!(outcome, LookupOutcome::Stale);
        assert_eq!(cache.len(), 0, "stale entry must be evicted");
    }

    #[test]
    fn appearing_shadow_at_probed_absent_path_invalidates() {
        // The case the whole design exists for: a BUNDLE winner while a
        // project slot is empty. The digest of the bundle winner never
        // changes, so only the recorded absence catches the new shadow.
        let dir = tempdir();
        let bundle_item = dir.join("bundle_item.py");
        let bundle_digest = write(&bundle_item, "# bundle winner");
        let project_slot = dir.join("project_slot.py"); // absent for now

        let cache = ResolutionCache::new(8);
        cache.insert(
            key(),
            output_with_root(ancestor(ItemSpace::Bundle, bundle_item, bundle_digest)),
            vec![ProbedAbsence {
                space: ItemSpace::Project,
                path: project_slot.clone(),
            }],
        );

        // Absence still holds → hit.
        assert_eq!(cache.get(&key()).1, LookupOutcome::Hit);

        // A project item appears where the winner was probed absent → the
        // resolution would now select a different (higher-precedence) winner.
        write(&project_slot, "# project shadow");
        assert_eq!(cache.get(&key()).1, LookupOutcome::Stale);
    }

    #[test]
    fn bundle_dependencies_are_not_revalidated() {
        // A bundle-space positive whose file does not even exist on disk still
        // validates: bundle content is immutable within the generation, which
        // is in the key. This is the genuinely-free tier.
        let cache = ResolutionCache::new(8);
        cache.insert(
            key(),
            output_with_root(ancestor(
                ItemSpace::Bundle,
                PathBuf::from("/nonexistent/bundle/item.py"),
                "digest-that-matches-nothing".into(),
            )),
            vec![ProbedAbsence {
                // A bundle-space absence is likewise not re-probed.
                space: ItemSpace::Bundle,
                path: PathBuf::from("/nonexistent/bundle/other.py"),
            }],
        );
        assert_eq!(cache.get(&key()).1, LookupOutcome::Hit);
    }

    #[test]
    fn generation_is_part_of_identity() {
        let cache = ResolutionCache::new(8);
        let mut k = key();
        cache.insert(
            k.clone(),
            output_with_root(ancestor(
                ItemSpace::Bundle,
                PathBuf::from("/b/item.py"),
                "d".into(),
            )),
            Vec::new(),
        );
        // A bumped generation is a different key: miss, not a stale hit.
        k.generation = 2;
        assert_eq!(cache.get(&k).1, LookupOutcome::Miss);
    }

    #[test]
    fn eviction_drops_the_oldest_and_keeps_the_newest() {
        let cache = ResolutionCache::new(2);
        let mut keys = Vec::new();
        for i in 0..3 {
            let mut k = key();
            k.canonical_ref = format!("tool:x{i}");
            keys.push(k.clone());
            cache.insert(
                k,
                output_with_root(ancestor(
                    ItemSpace::Bundle,
                    PathBuf::from("/b/item.py"),
                    "d".into(),
                )),
                Vec::new(),
            );
        }
        assert_eq!(cache.len(), 2, "capacity bound holds");
        // The two most-recently-inserted survive; the oldest was evicted.
        assert_eq!(cache.get(&keys[0]).1, LookupOutcome::Miss, "oldest evicted");
        assert_eq!(cache.get(&keys[1]).1, LookupOutcome::Hit, "newer kept");
        assert_eq!(cache.get(&keys[2]).1, LookupOutcome::Hit, "newest kept");
    }

    #[test]
    fn deleted_project_positive_invalidates() {
        let dir = tempdir();
        let path = dir.join("gone.py");
        let digest = write(&path, "# here");
        let cache = ResolutionCache::new(8);
        cache.insert(
            key(),
            output_with_root(ancestor(ItemSpace::Project, path.clone(), digest)),
            Vec::new(),
        );
        assert_eq!(cache.get(&key()).1, LookupOutcome::Hit);
        std::fs::remove_file(&path).unwrap();
        assert_eq!(
            cache.get(&key()).1,
            LookupOutcome::Stale,
            "deleted dep is stale"
        );
    }

    #[test]
    fn mixed_project_and_bundle_chain_revalidates_only_the_project_dep() {
        let dir = tempdir();
        let project_dep = dir.join("proj.py");
        let project_digest = write(&project_dep, "# v1");
        let cache = ResolutionCache::new(8);
        // root = project item; one bundle ancestor whose path does not exist.
        let mut output = output_with_root(ancestor(
            ItemSpace::Project,
            project_dep.clone(),
            project_digest,
        ));
        output.ancestors.push(ancestor(
            ItemSpace::Bundle,
            PathBuf::from("/nonexistent/bundle/parent.py"),
            "bundle-digest".into(),
        ));
        cache.insert(key(), output, Vec::new());
        // Bundle ancestor is never re-read (would fail); project dep unchanged → hit.
        assert_eq!(cache.get(&key()).1, LookupOutcome::Hit);
        // Change only the project dep → stale, proving the project dep IS checked.
        write(&project_dep, "# v2");
        assert_eq!(cache.get(&key()).1, LookupOutcome::Stale);
    }

    #[test]
    fn a_directory_at_a_probed_absent_path_does_not_invalidate() {
        let dir = tempdir();
        let bundle_item = dir.join("b.py");
        let bundle_digest = write(&bundle_item, "# bundle");
        let slot = dir.join("slot"); // will become a directory, not an item file
        let cache = ResolutionCache::new(8);
        cache.insert(
            key(),
            output_with_root(ancestor(ItemSpace::Bundle, bundle_item, bundle_digest)),
            vec![ProbedAbsence {
                space: ItemSpace::Project,
                path: slot.clone(),
            }],
        );
        assert_eq!(cache.get(&key()).1, LookupOutcome::Hit);
        // A directory is not a resolvable item — the resolver probes is_file(),
        // so this must NOT invalidate.
        std::fs::create_dir(&slot).unwrap();
        assert_eq!(cache.get(&key()).1, LookupOutcome::Hit);
    }
}
