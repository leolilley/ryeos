//! Garbage collection for CAS objects.
//!
//! Two-phase pipeline:
//! 1. **Compact** (opt-in, `--compact` flag): prune project snapshot DAGs
//!    per retention policy. Removes excess history, rewrites parent pointers.
//! 2. **Sweep** (always): mark-and-sweep unreachable CAS objects and blobs.
//!
//! Compact runs BEFORE sweep because compaction makes snapshots unreachable
//! (by removing them from the DAG). Sweep then deletes the orphaned objects.
//!
//! Submodules:
//! - `lock`: flock-based GC lock (prevents concurrent runs)
//! - `event_log`: JSONL append-only operational log
//! - `compact`: project snapshot DAG compaction with retention policy

pub mod compact;
pub mod event_log;
pub mod lock;
pub mod retention;

use std::path::Path;
use std::time::Instant;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::reachability;

// Re-export key types
pub use compact::CompactionResult;
pub use compact::RetentionPolicy;
pub use event_log::GcEvent;
pub use lock::GcLock;

/// GC parameters.
///
/// # Default vs deep profile (operational-hygiene decision)
///
/// A bare `ryeos gc` (all flags false) runs **only** the CAS mark-and-sweep —
/// it never touches runtime history, caches, trace output, or append-only logs.
/// The heavier reclamation is opt-in. Rather than flipping the manual default
/// to destructive (which would surprise operators), the decision is to keep the
/// **`deep` umbrella flag** (`--deep`) as the opt-in for cache purge, trace
/// truncation, and policy-driven terminal-chain retirement. Execution chain
/// heads are never removed as a directory operation; terminal-chain retirement
/// is coordinated one chain at a time by the daemon from captured policy.
/// Operational-history windows are separate, explicit parameters: Rust has no
/// fallback age or count, and omission disables that cleanup pass.
/// The individual `purge_cache` / `truncate_trace` / `prune_runtime_history`
/// flags remain available for targeted reclamation.
///
/// The signed scheduled-maintenance declaration runs the deep profile and
/// authors every operational cleanup window explicitly, so an
/// unattended install reclaims fully on cadence while interactive `ryeos gc`
/// stays conservative. Destructive-ish steps (trace truncation, fire pruning)
/// log loudly and report freed bytes so they're visible in the GC event log.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GcParams {
    /// Don't delete anything, just report.
    #[serde(default)]
    pub dry_run: bool,
    /// Compact project snapshot history before sweep.
    #[serde(default)]
    pub compact: bool,
    /// Deep profile: the opt-in umbrella for cache purge, trace truncation,
    /// and policy-driven terminal-chain retirement. Operational-history
    /// cleanup still requires its explicit window/count fields below. The
    /// scheduled maintenance fire sets both. Bare
    /// `ryeos gc` leaves it false and does a pure CAS sweep.
    ///
    /// Deep GC drops local runtime-only history/caches that can be rebuilt or
    /// are not part of operator config, node identity, installed bundles, or
    /// project/deployed heads.
    #[serde(default)]
    pub deep: bool,
    /// Purge rebuildable caches under `.ai/state/cache`. Request-owned
    /// `cache/executions` workspaces and lock-owned `cache/verified-code`
    /// generations are excluded because their in-memory lifelines are outside
    /// CAS reachability and own cleanup.
    #[serde(default)]
    pub purge_cache: bool,
    /// Truncate `.ai/state/trace-events.ndjson`.
    #[serde(default)]
    pub truncate_trace: bool,
    /// Run policy-driven terminal runtime-history retirement before CAS sweep.
    /// The daemon coordinator interprets this request and retires eligible
    /// chains individually after terminality and recovery-pin checks. This
    /// state-layer purge helper never removes chain heads itself.
    #[serde(default)]
    pub prune_runtime_history: bool,
    /// Maximum age of terminal schedule-fire groups. Omission disables the age
    /// bound; Rust deliberately supplies no default retention window.
    #[serde(default)]
    pub schedule_fire_max_age_days: Option<u64>,
    /// Maximum number of terminal schedule-fire groups kept per schedule.
    /// Omission disables the count bound.
    #[serde(default)]
    pub schedule_fire_max_count: Option<usize>,
    /// Age after which terminal sync-job rows and their attempts are removed.
    /// Omission disables sync-job retention.
    #[serde(default)]
    pub sync_job_retention_days: Option<u64>,
    /// Grace after a seat lease expires before its running session is settled.
    /// Omission disables automatic seat settlement.
    #[serde(default)]
    pub seat_lease_grace_seconds: Option<u64>,
    /// Maximum age of an abandoned durable multi-request CAS upload stage.
    /// Omission preserves every stage indefinitely; Rust supplies no fallback.
    #[serde(default)]
    pub durable_cas_upload_max_age_seconds: Option<u64>,
    /// Complete authored retention policy for project-history compaction.
    /// Required whenever `compact` is true; Rust supplies no fallback.
    #[serde(default)]
    pub policy: Option<RetentionPolicy>,
}

impl GcParams {
    pub fn validate(&self) -> Result<()> {
        if self.compact && self.policy.is_none() {
            anyhow::bail!("compact=true requires policy.manual_pushes and policy.auto_snapshots");
        }
        Ok(())
    }
}

#[cfg(test)]
mod params_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn empty_gc_params_deserialize_to_default() {
        let params: GcParams = serde_json::from_value(json!({})).unwrap();
        assert!(!params.dry_run);
        assert!(!params.compact);
        assert!(params.schedule_fire_max_age_days.is_none());
        assert!(params.schedule_fire_max_count.is_none());
        assert!(params.sync_job_retention_days.is_none());
        assert!(params.seat_lease_grace_seconds.is_none());
        assert!(params.durable_cas_upload_max_age_seconds.is_none());
        assert!(params.policy.is_none());
    }

    #[test]
    fn operational_cleanup_params_are_strict_and_explicit() {
        let params: GcParams = serde_json::from_value(json!({
            "schedule_fire_max_age_days": 30,
            "schedule_fire_max_count": 500,
            "sync_job_retention_days": 14,
            "seat_lease_grace_seconds": 600,
            "durable_cas_upload_max_age_seconds": 1234
        }))
        .unwrap();
        assert_eq!(params.schedule_fire_max_age_days, Some(30));
        assert_eq!(params.schedule_fire_max_count, Some(500));
        assert_eq!(params.sync_job_retention_days, Some(14));
        assert_eq!(params.seat_lease_grace_seconds, Some(600));
        assert_eq!(params.durable_cas_upload_max_age_seconds, Some(1_234));

        assert!(serde_json::from_value::<GcParams>(json!({
            "schedule_fire_max_count": -1
        }))
        .is_err());
        assert!(serde_json::from_value::<GcParams>(json!({
            "sync_job_retention_days": "14"
        }))
        .is_err());
        assert!(serde_json::from_value::<GcParams>(json!({
            "runtime_retention_days": 14
        }))
        .is_err());
    }

    #[test]
    fn compaction_requires_a_complete_nested_policy() {
        let missing: GcParams = serde_json::from_value(json!({ "compact": true })).unwrap();
        assert!(missing.validate().is_err());
        assert!(serde_json::from_value::<GcParams>(json!({
            "compact": true,
            "policy": { "manual_pushes": 10 }
        }))
        .is_err());

        let complete: GcParams = serde_json::from_value(json!({
            "compact": true,
            "policy": { "manual_pushes": 10, "auto_snapshots": 30 }
        }))
        .unwrap();
        complete.validate().unwrap();
    }
}

/// Full GC result.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GcResult {
    pub roots_walked: usize,
    pub reachable_objects: usize,
    pub reachable_blobs: usize,
    pub deleted_objects: usize,
    pub deleted_blobs: usize,
    /// Abandoned files from an interrupted CAS atomic publication.
    pub deleted_cas_staging_files: usize,
    pub deleted_runtime_files: usize,
    /// Schedule-fire JSONL lines dropped by the retention sweep.
    #[serde(default)]
    pub deleted_fire_records: usize,
    /// Terminal rows dropped from the scheduler fire projection.
    #[serde(default)]
    pub deleted_fire_projection_rows: usize,
    /// Terminal sync-job rows dropped by the retention sweep.
    #[serde(default)]
    pub deleted_sync_jobs: usize,
    /// Sync-job attempt rows dropped alongside their retired jobs.
    #[serde(default)]
    pub deleted_sync_job_attempts: usize,
    /// Running seat sessions settled after their runtime lease expired.
    #[serde(default)]
    pub reaped_seats: usize,
    /// Captured-policy terminal chains which were due when the sweep ran.
    #[serde(default)]
    pub terminal_chain_candidates: usize,
    /// Captured-policy terminal chains retired after the authoritative recheck.
    #[serde(default)]
    pub retired_terminal_chains: usize,
    /// Thread rows removed from the disposable projection.
    #[serde(default)]
    pub deleted_thread_projection_rows: usize,
    /// Per-thread/chain rows removed from the runtime database.
    #[serde(default)]
    pub deleted_thread_runtime_rows: usize,
    /// Per-thread runtime/checkpoint files removed for retired chains.
    #[serde(default)]
    pub deleted_thread_runtime_files: usize,
    /// Interrupted journaled removals completed before the candidate sweep.
    #[serde(default)]
    pub pending_retirements_recovered: usize,
    /// Abandoned durable CAS upload stages retired by an explicit signed age.
    #[serde(default)]
    pub retired_durable_cas_uploads: usize,
    /// Materialized snapshot generations inspected by daemon-coordinated deep GC.
    pub inspected_materialized_snapshots: usize,
    /// Inactive materialized snapshot generations removed, or eligible in dry-run.
    pub deleted_materialized_snapshots: usize,
    /// Materialized generations preserved because their exact lease was active.
    pub preserved_active_materialized_snapshots: usize,
    pub freed_bytes: u64,
    pub compaction: Option<CompactionResult>,
    pub duration_ms: u64,
}

/// Durable operational CAS roots that are not yet represented by signed refs.
/// Object and blob identities remain distinct so a blob is never interpreted
/// as a JSON closure root.
#[derive(Debug, Clone, Default)]
pub struct AdditionalCasRoots {
    pub object_hashes: Vec<String>,
    pub blob_hashes: Vec<String>,
}

/// Purge opt-in runtime-only state before CAS sweep.
///
/// The caller should hold the daemon write barrier. This helper deliberately
/// stays away from `.ai/config`, `.ai/node`, `.ai/bundles`, project heads, and
/// deployed project heads. `deep` is a convenience flag that enables every
/// runtime purge below.
pub fn purge_runtime_state(
    runtime_state_dir: &Path,
    params: &GcParams,
    fire_retention: Option<retention::FireRetentionPolicy>,
    result: &mut GcResult,
) -> Result<Vec<retention::FireRetentionTarget>> {
    let runtime_directory = lillux::PinnedDirectory::open(runtime_state_dir)?
        .ok_or_else(|| anyhow::anyhow!("runtime-state directory is absent"))?;
    purge_runtime_state_in_directory(&runtime_directory, params, fire_retention, result)
}

/// Purge runtime-only state beneath the exact runtime directory selected by
/// the owning state authority.
pub fn purge_runtime_state_in_directory(
    runtime_directory: &lillux::PinnedDirectory,
    params: &GcParams,
    fire_retention: Option<retention::FireRetentionPolicy>,
    result: &mut GcResult,
) -> Result<Vec<retention::FireRetentionTarget>> {
    // Chain heads and per-thread runtime/checkpoint state are deliberately not
    // touched here. The daemon owns the cross-store liveness view required to
    // retire one captured-policy-eligible terminal chain safely. Blind removal
    // of `refs/generic/chains` makes the projection lie and can discard live or
    // resumable work.

    if params.deep || params.purge_cache {
        let cache_name = std::ffi::OsStr::new("cache");
        if let Some(cache_directory) = runtime_directory.open_child_directory(cache_name)? {
            remove_rebuildable_cache_directory(&cache_directory, params.dry_run, result)?;
        } else if !params.dry_run {
            runtime_directory
                .open_or_create_child(cache_name, 0o700)
                .context("failed to establish runtime cache directory")?;
        }
    }

    if params.deep || params.truncate_trace {
        let trace_path = runtime_directory.path().join("trace-events.ndjson");
        let before = result.freed_bytes;
        truncate_file_in_directory(
            runtime_directory,
            std::ffi::OsStr::new("trace-events.ndjson"),
            params.dry_run,
            result,
        )?;
        if result.freed_bytes > before {
            tracing::warn!(
                path = %trace_path.display(),
                freed_bytes = result.freed_bytes - before,
                dry_run = params.dry_run,
                "GC: truncating daemon trace log"
            );
        }
    }

    // Retention sweep: age/count-bound the append-only schedule fire history.
    // Each bound is independently content-authored; with both omitted this
    // pass is disabled, even for deep GC.
    let fire_retention_targets = retention::sweep_fire_jsonl_in_directory(
        runtime_directory,
        fire_retention,
        params.dry_run,
        result,
    )?;

    Ok(fire_retention_targets)
}

/// Full GC pipeline: compact (optional) → sweep.
///
/// Compact runs first to make snapshot DAG orphans, then sweep collects
/// the final reachable set and deletes everything else.
///
/// `signer` is required only when `compact=true` (to update project head refs).
/// Sweep-only GC doesn't need signing authority.
pub fn run_gc(
    runtime_state_dir: &Path,
    trust_store: &crate::refs::TrustStore,
    signer: Option<&dyn crate::Signer>,
    params: &GcParams,
) -> Result<GcResult> {
    run_gc_with_additional_roots(
        runtime_state_dir,
        trust_store,
        signer,
        params,
        &AdditionalCasRoots::default(),
    )
}

/// Run GC while preserving complete operational closures which are not yet
/// reachable from a signed ref, notably pending chain-head Set targets.
///
/// The exclusive guard here is a final safety net. A daemon coordinator may
/// already hold the same guard while it quiesces writers and snapshots the
/// additional roots; same-thread exclusive acquisition is reentrant.
pub fn run_gc_with_additional_roots(
    runtime_state_dir: &Path,
    trust_store: &crate::refs::TrustStore,
    signer: Option<&dyn crate::Signer>,
    params: &GcParams,
    additional_roots: &AdditionalCasRoots,
) -> Result<GcResult> {
    params.validate()?;
    let runtime_directory = lillux::PinnedDirectory::open(runtime_state_dir)?
        .ok_or_else(|| anyhow::anyhow!("runtime-state directory is absent"))?;
    let cas_mutation_guard = if params.dry_run {
        crate::recovery::CasMutationGuard::acquire_existing_exclusive_in_pinned_runtime(
            &runtime_directory,
        )?
    } else {
        crate::recovery::CasMutationGuard::acquire_exclusive_in_pinned_runtime(&runtime_directory)?
    };
    let authority = crate::PinnedStateAuthority::from_pinned_runtime(
        runtime_directory,
        std::sync::Arc::new(trust_store.clone()),
        !params.dry_run,
    )?;
    authority.ensure_guard(&cas_mutation_guard)?;
    run_gc_with_pinned_authority(
        &authority,
        &cas_mutation_guard,
        signer,
        params,
        additional_roots,
    )
}

/// Run the online GC pipeline against the exact state authority captured from
/// the live [`crate::StateDb`].
///
/// The caller owns the already-acquired exclusive CAS guard and quiesced write
/// barrier. This function never reconstructs runtime, refs, recovery, or CAS
/// authority from a pathname.
pub fn run_gc_with_pinned_authority(
    authority: &crate::PinnedStateAuthority,
    cas_mutation_guard: &crate::recovery::CasMutationGuard,
    signer: Option<&dyn crate::Signer>,
    params: &GcParams,
    additional_roots: &AdditionalCasRoots,
) -> Result<GcResult> {
    params.validate()?;
    if !cas_mutation_guard.is_exclusive() {
        anyhow::bail!("GC requires an exclusive CAS mutation guard");
    }
    authority.ensure_guard(cas_mutation_guard)?;
    let started = Instant::now();
    let mut result = GcResult::default();
    let recovery = authority.recovery();
    let cas = authority.cas_store()?;

    if !params.dry_run {
        if let Some(max_age_seconds) = params.durable_cas_upload_max_age_seconds {
            let cutoff = iso8601_seconds_ago(max_age_seconds);
            result.retired_durable_cas_uploads = authority
                .require_recovery()?
                .retire_durable_cas_uploads_created_before(&cutoff, cas_mutation_guard)
                .context("retire abandoned durable CAS upload stages")?;
        }
    }
    let capture_cleanup = cas
        .prune_abandoned_blob_captures(params.dry_run)
        .context("prune interrupted streaming CAS blob captures")?;
    result.deleted_cas_staging_files = result
        .deleted_cas_staging_files
        .saturating_add(capture_cleanup.files);
    result.freed_bytes = result.freed_bytes.saturating_add(capture_cleanup.bytes);
    let staged_roots = match (recovery, params.dry_run) {
        (Some(recovery), true) => recovery.inspect_staged_cas_root_hashes_read_only()?,
        (Some(recovery), false) => recovery.active_staged_cas_root_hashes()?,
        (None, true) => crate::recovery::StagedCasRootHashes::default(),
        (None, false) => anyhow::bail!("mutable GC requires recovery authority"),
    };
    let mut operational_roots = additional_roots
        .object_hashes
        .iter()
        .cloned()
        .collect::<std::collections::BTreeSet<_>>();
    if let Some(recovery) = recovery {
        operational_roots.extend(
            recovery
                .pending_transition_object_roots()
                .context("read pending chain-head transition roots before CAS sweep")?,
        );
    }
    operational_roots.extend(staged_roots.object_hashes.iter().cloned());

    if params.compact {
        let compact_signer = signer.ok_or_else(|| {
            anyhow::anyhow!("--compact requires a signer (use --key to provide one)")
        })?;
        let policy = params
            .policy
            .as_ref()
            .expect("GcParams::validate requires policy when compact=true");
        result.compaction = Some(compact::compact_projects_pinned(
            &cas,
            authority.refs_directory(),
            authority.trust_store(),
            compact_signer,
            policy,
            params.dry_run,
        )?);
    }

    let mut reachable = reachability::collect_reachable_pinned(
        &cas,
        authority.refs_directory(),
        authority.trust_store(),
    )?;
    if !operational_roots.is_empty() {
        let closure = crate::object_closure::collect_object_closure_with_cas(
            &cas,
            operational_roots.iter().cloned(),
        )?;
        if !closure.is_complete() {
            anyhow::bail!(
                "additional GC root closure is incomplete: missing_objects={}, missing_blobs={}, malformed_objects={}, unsupported_objects={}",
                closure.missing_objects.len(),
                closure.missing_blobs.len(),
                closure.malformed_objects.len(),
                closure.unsupported_objects.len(),
            );
        }
        reachable.object_hashes.extend(closure.object_hashes);
        reachable.blob_hashes.extend(closure.blob_hashes);
    }
    reachable
        .blob_hashes
        .extend(staged_roots.blob_hashes.iter().cloned());
    for hash in &additional_roots.blob_hashes {
        if !lillux::valid_hash(hash) || hash.bytes().any(|byte| byte.is_ascii_uppercase()) {
            anyhow::bail!("invalid additional GC blob root: {hash}");
        }
        cas.get_blob(hash)?
            .ok_or_else(|| anyhow::anyhow!("additional GC blob root is absent: {hash}"))?;
        reachable.blob_hashes.insert(hash.clone());
    }

    result.roots_walked = reachable.authoritative_root_count
        + operational_roots.len()
        + staged_roots.blob_hashes.len()
        + additional_roots.blob_hashes.len();
    result.reachable_objects = reachable.object_hashes.len();
    result.reachable_blobs = reachable.blob_hashes.len();

    sweep_sharded_directory(
        authority.cas_directory(),
        "objects",
        ".json",
        &reachable.object_hashes,
        params.dry_run,
        &mut result,
    )?;
    sweep_sharded_directory(
        authority.cas_directory(),
        "blobs",
        "",
        &reachable.blob_hashes,
        params.dry_run,
        &mut result,
    )?;

    result.duration_ms = started.elapsed().as_millis() as u64;
    Ok(result)
}

fn iso8601_seconds_ago(seconds: u64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    lillux::time::iso8601_from_unix_secs(now.saturating_sub(seconds))
}

/// Sweep a sharded directory, deleting files not in the reachable set.
///
/// The shard layout is `namespace/ab/cd/hash{ext}` (two-level hex sharding).
#[cfg(test)]
fn sweep_sharded_dir(
    cas_root: &Path,
    namespace: &str,
    ext: &str,
    reachable: &std::collections::HashSet<String>,
    dry_run: bool,
    result: &mut GcResult,
) -> Result<()> {
    let Some(cas_directory) = lillux::PinnedDirectory::open(cas_root)? else {
        return Ok(());
    };
    sweep_sharded_directory(&cas_directory, namespace, ext, reachable, dry_run, result)
}

fn sweep_sharded_directory(
    cas_directory: &lillux::PinnedDirectory,
    namespace: &str,
    ext: &str,
    reachable: &std::collections::HashSet<String>,
    dry_run: bool,
    result: &mut GcResult,
) -> Result<()> {
    let dir = cas_directory.path().join(namespace);
    let Some(namespace_dir) =
        cas_directory.open_child_directory(std::ffi::OsStr::new(namespace))?
    else {
        return Ok(());
    };

    for shard1_name in namespace_dir.entry_names()? {
        let shard1_text = shard1_name
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("non-UTF8 CAS shard under {}", dir.display()))?;
        if lillux::cas::is_reserved_namespace_entry(namespace, shard1_text) {
            namespace_dir
                .open_child_directory(&shard1_name)?
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "reserved CAS namespace entry is not a directory: {}",
                        namespace_dir.path().join(&shard1_name).display()
                    )
                })?;
            continue;
        }
        validate_cas_shard_component(shard1_text, &dir)?;
        let shard1 = namespace_dir
            .open_child_directory(&shard1_name)?
            .ok_or_else(|| anyhow::anyhow!("CAS shard is not a directory: {shard1_text}"))?;

        for shard2_name in shard1.entry_names()? {
            let shard2_text = shard2_name.to_str().ok_or_else(|| {
                anyhow::anyhow!("non-UTF8 CAS sub-shard under {}", shard1.path().display())
            })?;
            validate_cas_shard_component(shard2_text, shard1.path())?;
            let shard2 = shard1.open_child_directory(&shard2_name)?.ok_or_else(|| {
                anyhow::anyhow!("CAS sub-shard is not a directory: {shard2_text}")
            })?;

            for file_name in shard2.entry_names()? {
                let file = match shard2.open_entry(&file_name, false)? {
                    Some(lillux::PinnedDirectoryEntry::Directory(_)) => anyhow::bail!(
                        "CAS leaf contains unexpected directory: {}",
                        shard2.path().join(&file_name).display()
                    ),
                    Some(lillux::PinnedDirectoryEntry::Regular(file)) => file,
                    None => anyhow::bail!(
                        "CAS leaf entry disappeared: {}",
                        shard2.path().join(&file_name).display()
                    ),
                };
                let filename = file_name.to_str().ok_or_else(|| {
                    anyhow::anyhow!("non-UTF8 CAS filename under {}", shard2.path().display())
                })?;
                let atomic_staging_hash = incomplete_atomic_write_temp_hash(filename, ext);
                if let Some(hash) = atomic_staging_hash {
                    if !canonical_cas_hash_at_shard(hash, shard1_text, shard2_text) {
                        anyhow::bail!(
                            "CAS atomic staging entry is not stored at its canonical shard path: {}",
                            shard2.path().join(&file_name).display()
                        );
                    }
                }
                if is_incomplete_batch_temp(filename) || atomic_staging_hash.is_some() {
                    let file_size = file.metadata()?.len();
                    if dry_run {
                        tracing::info!(
                            namespace,
                            path = %shard2.path().join(&file_name).display(),
                            size = file_size,
                            "would delete incomplete CAS batch temp"
                        );
                    } else {
                        shard2.remove_if_same(&file_name, &file).with_context(|| {
                            format!(
                                "delete incomplete CAS batch temp {}",
                                shard2.path().join(&file_name).display()
                            )
                        })?;
                    }
                    result.deleted_cas_staging_files += 1;
                    result.freed_bytes += file_size;
                    continue;
                }
                let hash = if ext.is_empty() {
                    filename
                } else {
                    filename.strip_suffix(ext).ok_or_else(|| {
                        anyhow::anyhow!("unexpected CAS object filename: {filename}")
                    })?
                };
                if !canonical_cas_hash_at_shard(hash, shard1_text, shard2_text) {
                    anyhow::bail!(
                        "CAS entry is not stored at its canonical shard path: {}",
                        shard2.path().join(&file_name).display()
                    );
                }

                if !reachable.contains(hash) {
                    let file_size = file.metadata()?.len();
                    if dry_run {
                        tracing::info!(
                            namespace,
                            hash = %&hash[..16],
                            size = file_size,
                            "would delete (dry run)"
                        );
                    } else {
                        shard2.remove_if_same(&file_name, &file).with_context(|| {
                            format!(
                                "delete unreachable CAS entry {}",
                                shard2.path().join(&file_name).display()
                            )
                        })?;
                    }

                    if namespace == "blobs" {
                        result.deleted_blobs += 1;
                    } else {
                        result.deleted_objects += 1;
                    }
                    result.freed_bytes += file_size;
                }
            }
            if !dry_run {
                shard1.remove_empty_child_if_same(&shard2_name, &shard2)?;
            }
        }
        if !dry_run {
            namespace_dir.remove_empty_child_if_same(&shard1_name, &shard1)?;
        }
    }

    Ok(())
}

fn incomplete_atomic_write_temp_hash<'a>(filename: &'a str, ext: &str) -> Option<&'a str> {
    let rest = filename.strip_prefix('.')?;
    let (target, suffix) = rest.rsplit_once(".tmp.")?;
    let (pid, sequence) = suffix.split_once('.')?;
    if pid.is_empty()
        || sequence.is_empty()
        || sequence.contains('.')
        || !pid.bytes().all(|byte| byte.is_ascii_digit())
        || !sequence.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    if ext.is_empty() {
        Some(target)
    } else {
        target.strip_suffix(ext)
    }
}

fn canonical_cas_hash_at_shard(hash: &str, shard1: &str, shard2: &str) -> bool {
    lillux::valid_hash(hash)
        && !hash.bytes().any(|byte| byte.is_ascii_uppercase())
        && &hash[0..2] == shard1
        && &hash[2..4] == shard2
}

fn is_incomplete_batch_temp(filename: &str) -> bool {
    let Some(rest) = filename.strip_prefix(".secure.tmp.") else {
        return false;
    };
    let Some((pid, sequence)) = rest.split_once('.') else {
        return false;
    };
    !pid.is_empty()
        && !sequence.is_empty()
        && !sequence.contains('.')
        && pid.bytes().all(|byte| byte.is_ascii_digit())
        && sequence.bytes().all(|byte| byte.is_ascii_digit())
}

fn validate_cas_shard_component(component: &str, parent: &Path) -> Result<()> {
    if component.len() != 2
        || !component
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        anyhow::bail!(
            "invalid CAS shard component `{component}` under {}",
            parent.display()
        );
    }
    Ok(())
}

fn remove_directory_contents(
    directory: &lillux::PinnedDirectory,
    dry_run: bool,
    result: &mut GcResult,
) -> Result<()> {
    for name in directory.entry_names()? {
        match directory.open_entry(&name, false)? {
            Some(lillux::PinnedDirectoryEntry::Directory(child)) => {
                remove_directory_contents(&child, dry_run, result)?;
                if !dry_run {
                    let removed = directory.remove_empty_child_if_same(&name, &child)?;
                    if !removed {
                        anyhow::bail!(
                            "runtime directory changed while being purged: {}",
                            child.path().display()
                        );
                    }
                }
            }
            Some(lillux::PinnedDirectoryEntry::Regular(file)) => {
                let file_size = file.metadata()?.len();
                if !dry_run {
                    directory.remove_if_same(&name, &file)?;
                }
                result.deleted_runtime_files += 1;
                result.freed_bytes += file_size;
            }
            None => anyhow::bail!(
                "runtime purge entry disappeared: {}",
                directory.path().join(&name).display()
            ),
        }
    }
    Ok(())
}

/// Remove rebuildable cache entries without touching live execution roots.
///
/// `cache/executions` contains pushed-snapshot checkouts and isolated
/// no-project workspaces whose `TempDirGuard`s own cleanup. The isolation's
/// `cache/verified-code` generations are likewise protected by process-held
/// lifetime locks and remove themselves when their runtime generation drops.
/// Materialized snapshots are owned by the executor and require its exact
/// construction/lease protocol; the daemon maintenance coordinator invokes
/// that lease-aware sweep separately. This generic state layer cannot infer
/// those liveness contracts, so it must never recursively unlink them.
fn remove_rebuildable_cache_directory(
    cache_directory: &lillux::PinnedDirectory,
    dry_run: bool,
    result: &mut GcResult,
) -> Result<()> {
    for name in cache_directory.entry_names()? {
        if matches!(
            name.to_str(),
            Some("executions" | "verified-code" | "snapshots")
        ) {
            continue;
        }
        match cache_directory.open_entry(&name, false)? {
            Some(lillux::PinnedDirectoryEntry::Directory(child)) => {
                remove_directory_contents(&child, dry_run, result)?;
                if !dry_run && !cache_directory.remove_empty_child_if_same(&name, &child)? {
                    anyhow::bail!(
                        "runtime cache directory changed while being purged: {}",
                        child.path().display()
                    );
                }
            }
            Some(lillux::PinnedDirectoryEntry::Regular(file)) => {
                let file_size = file.metadata()?.len();
                if !dry_run {
                    cache_directory.remove_if_same(&name, &file)?;
                }
                result.deleted_runtime_files += 1;
                result.freed_bytes += file_size;
            }
            None => anyhow::bail!(
                "runtime cache entry disappeared: {}",
                cache_directory.path().join(&name).display()
            ),
        }
    }
    Ok(())
}

fn truncate_file_in_directory(
    directory: &lillux::PinnedDirectory,
    name: &std::ffi::OsStr,
    dry_run: bool,
    result: &mut GcResult,
) -> Result<()> {
    let path = directory.path().join(name);
    let Some(file) = directory.open_regular(name, !dry_run)? else {
        return Ok(());
    };
    let file_size = file.metadata()?.len();
    if !dry_run {
        file.set_len(0)
            .with_context(|| format!("failed to truncate {}", path.display()))?;
        file.sync_all()
            .with_context(|| format!("failed to sync truncated file {}", path.display()))?;
    }
    result.deleted_runtime_files += 1;
    result.freed_bytes += file_size;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gc_result_default() {
        let result = GcResult::default();
        assert_eq!(result.deleted_objects, 0);
        assert_eq!(result.reachable_blobs, 0);
    }

    #[test]
    fn retention_policy_requires_both_authored_fields() {
        let policy: RetentionPolicy = serde_json::from_value(serde_json::json!({
            "manual_pushes": 10,
            "auto_snapshots": 30
        }))
        .unwrap();
        assert_eq!(policy.manual_pushes, 10);
        assert_eq!(policy.auto_snapshots, 30);
        assert!(
            serde_json::from_value::<RetentionPolicy>(serde_json::json!({
                "manual_pushes": 10
            }))
            .is_err()
        );
    }

    #[test]
    fn gc_params_construction() {
        let params = GcParams {
            dry_run: true,
            compact: false,
            policy: None,
            ..GcParams::default()
        };
        assert!(params.dry_run);
        assert!(!params.compact);
        assert!(params.policy.is_none());
    }

    /// Integration test: compact then sweep clears compacted victims.
    ///
    /// Creates a project snapshot chain, compacts it (removing some snapshots),
    /// then runs a full GC. The removed snapshots should be swept as unreachable.
    #[test]
    fn compact_then_sweep_cleans_victims() {
        use crate::refs;
        use crate::signer::TestSigner;
        use crate::Signer as _;
        use std::fs;

        let tmp = tempfile::tempdir().unwrap();
        let cas_root = tmp.path().join("objects");
        let refs_root = tmp.path().join("refs");
        fs::create_dir_all(&cas_root).unwrap();
        fs::create_dir_all(&refs_root).unwrap();
        let signer = TestSigner::default();
        let mut trust_store = crate::refs::TrustStore::new();
        trust_store.insert(signer.fingerprint().to_string(), signer.verifying_key());

        fn write_object(cas_root: &std::path::Path, value: &serde_json::Value) -> String {
            let canonical = lillux::canonical_json(value).unwrap();
            let hash = lillux::sha256_hex(canonical.as_bytes());
            let path = lillux::shard_path(cas_root, "objects", &hash, ".json");
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            lillux::atomic_write(&path, canonical.as_bytes()).unwrap();
            hash
        }

        // Write a chain: snap5 -> snap4 -> snap3 -> snap2 -> snap1 (all auto)
        let tree_hash = write_object(
            &cas_root,
            &serde_json::json!({
                "kind": "project_tree",
                "schema": crate::objects::ProjectTree::SCHEMA,
                "files": {},
            }),
        );
        let policy_hash = write_object(
            &cas_root,
            &crate::objects::ProjectSnapshotPolicy::new(
                crate::project_sync::ProjectSyncScope::FullProject,
                Vec::new(),
                Vec::new(),
                std::collections::BTreeMap::new(),
            )
            .unwrap()
            .to_value(),
        );
        let mut hashes = Vec::new();
        for i in 0..5 {
            let parents = hashes.last().cloned().into_iter().collect::<Vec<String>>();
            let hash = write_object(
                &cas_root,
                &serde_json::json!({
                    "kind": "project_snapshot",
                    "schema": crate::objects::ProjectSnapshot::SCHEMA,
                    "project_tree_hash": tree_hash,
                    "effective_policy_hash": policy_hash,
                    "message": null,
                    "parent_hashes": parents,
                    "created_at": format!("2026-04-23T00:00:0{i}Z"),
                    "source": "fold_back",
                }),
            );
            hashes.push(hash);
        }

        // Set HEAD to snap5
        let project_lock =
            refs::ProjectHeadLock::acquire(&refs_root, "fp:test-principal", "victim-proj").unwrap();
        refs::write_verified_project_head_ref(
            &refs_root,
            "fp:test-principal",
            "victim-proj",
            &hashes[4],
            &signer,
            &trust_store,
            &project_lock,
        )
        .unwrap();
        drop(project_lock);

        // Count objects before GC
        let count_before = count_objects(&cas_root);
        assert!(count_before >= 6); // 5 snapshots + their shared manifest

        // Verify compaction directly (dry run first to check logic)
        let policy = RetentionPolicy {
            manual_pushes: 10,
            auto_snapshots: 1,
        };
        let dry_compact =
            compact::compact_projects(&cas_root, &refs_root, &trust_store, &signer, &policy, true)
                .unwrap();
        assert_eq!(dry_compact.projects_scanned, 1);
        assert_eq!(
            dry_compact.snapshots_removed, 3,
            "dry run should remove 3 snapshots"
        );

        // Run GC with compact (keep HEAD + 1 auto = 2 snapshots, remove 3)
        let params = GcParams {
            dry_run: false,
            compact: true,
            policy: Some(policy),
            ..GcParams::default()
        };

        let result = run_gc(tmp.path(), &trust_store, Some(&signer), &params).unwrap();

        assert!(result.compaction.is_some());
        let compaction = result.compaction.unwrap();
        assert_eq!(compaction.snapshots_removed, 3);
        assert!(
            result.deleted_objects >= 3,
            "should have deleted at least 3 unreachable snapshots"
        );

        let count_after = count_objects(&cas_root);
        assert!(
            count_after < count_before,
            "expected fewer objects after GC: before={}, after={}, deleted={}",
            count_before,
            count_after,
            result.deleted_objects
        );
    }

    /// Sweep-only GC (no compact) on empty CAS is a no-op.
    #[test]
    fn sweep_only_empty_cas() {
        use crate::signer::TestSigner;
        use std::fs;

        let tmp = tempfile::tempdir().unwrap();
        let cas_root = tmp.path().join("objects");
        let refs_root = tmp.path().join("refs");
        fs::create_dir_all(&cas_root).unwrap();
        fs::create_dir_all(&refs_root).unwrap();

        let signer = TestSigner::default();
        let trust_store = crate::refs::TrustStore::new();
        let params = GcParams {
            dry_run: false,
            compact: false,
            policy: None,
            ..GcParams::default()
        };

        let result = run_gc(tmp.path(), &trust_store, Some(&signer), &params).unwrap();
        assert_eq!(result.deleted_objects, 0);
        assert_eq!(result.deleted_blobs, 0);
        assert!(result.compaction.is_none());
    }

    #[test]
    fn sweep_intrinsically_preserves_pending_transition_closure() {
        use std::fs;

        let tmp = tempfile::tempdir().unwrap();
        let runtime_state_dir = tmp.path().join("state");
        let cas_root = runtime_state_dir.join("objects");
        let refs_root = runtime_state_dir.join("refs");
        fs::create_dir_all(&cas_root).unwrap();
        fs::create_dir_all(&refs_root).unwrap();

        let write_object = |value: serde_json::Value| {
            let canonical = lillux::canonical_json(&value).unwrap();
            let hash = lillux::sha256_hex(canonical.as_bytes());
            let path = lillux::shard_path(&cas_root, "objects", &hash, ".json");
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            lillux::atomic_write(&path, canonical.as_bytes()).unwrap();
            (hash, path)
        };
        let (pending_hash, pending_path) = write_object(serde_json::json!({
            "kind": "source_manifest",
            "item_source_hashes": {},
        }));
        let (_unreachable_hash, unreachable_path) = write_object(serde_json::json!({
            "kind": "source_manifest",
            "item_source_hashes": {"different": pending_hash},
        }));

        let recovery =
            crate::recovery::RecoveryStore::from_runtime_state_dir(&runtime_state_dir).unwrap();
        let cas_guard =
            crate::recovery::CasMutationGuard::acquire_shared(&runtime_state_dir).unwrap();
        let chain_lock = crate::chain::ChainLock::acquire(&refs_root, "T-pending").unwrap();
        recovery
            .prepare_set(&chain_lock, "T-pending", None, &pending_hash)
            .unwrap();
        drop(chain_lock);
        drop(cas_guard);

        let result = run_gc(
            &runtime_state_dir,
            &crate::refs::TrustStore::new(),
            None,
            &GcParams::default(),
        )
        .unwrap();

        assert!(pending_path.is_file());
        assert!(!unreachable_path.exists());
        assert_eq!(result.deleted_objects, 1);
        assert_eq!(result.roots_walked, 1);
    }

    fn count_objects(cas_root: &std::path::Path) -> usize {
        let objects_dir = cas_root.join("objects");
        if !objects_dir.is_dir() {
            return 0;
        }
        let mut count = 0;
        count_objects_recursive(&objects_dir, &mut count);
        count
    }

    fn count_objects_recursive(dir: &std::path::Path, count: &mut usize) {
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                if entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false) {
                    count_objects_recursive(&entry.path(), count);
                } else if entry.file_type().map(|ft| ft.is_file()).unwrap_or(false) {
                    *count += 1;
                }
            }
        }
    }

    /// Shard-path contract test: verify that sweep finds files created by
    /// `lillux::shard_path`. This guards against shard depth mismatches
    /// (the prior regression where sweep was 2-level but layout was 3-level).
    #[test]
    fn sweep_finds_files_created_by_lillux_shard_path() {
        use std::collections::HashSet;
        use std::fs;

        let tmp = tempfile::tempdir().unwrap();
        let cas_root = tmp.path().join("cas");

        // Use valid 64-char hex hashes (as required by lillux::shard_path)
        let object_hashes: Vec<String> = vec![
            lillux::sha256_hex(b"test-object-1"),
            lillux::sha256_hex(b"test-object-2"),
            lillux::sha256_hex(b"test-object-3"),
        ];
        for hash in &object_hashes {
            let path = lillux::shard_path(&cas_root, "objects", hash, ".json");
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, b"{}").unwrap();
        }

        let blob_hash = lillux::sha256_hex(b"test-blob-1");
        let blob_path = lillux::shard_path(&cas_root, "blobs", &blob_hash, "");
        fs::create_dir_all(blob_path.parent().unwrap()).unwrap();
        fs::write(&blob_path, b"content").unwrap();

        // Run sweep with empty reachable set — everything should be deleted
        let reachable_objects: HashSet<String> = HashSet::new();
        let reachable_blobs: HashSet<String> = HashSet::new();
        let mut result = GcResult::default();

        sweep_sharded_dir(
            &cas_root,
            "objects",
            ".json",
            &reachable_objects,
            false,
            &mut result,
        )
        .unwrap();
        sweep_sharded_dir(&cas_root, "blobs", "", &reachable_blobs, false, &mut result).unwrap();

        assert_eq!(
            result.deleted_objects, 3,
            "sweep should find all 3 objects created by shard_path"
        );
        assert_eq!(
            result.deleted_blobs, 1,
            "sweep should find the blob created by shard_path"
        );

        // Verify files are actually gone
        for hash in &object_hashes {
            let path = lillux::shard_path(&cas_root, "objects", hash, ".json");
            assert!(
                !path.exists(),
                "object file should be deleted: {}",
                path.display()
            );
        }
        assert!(!blob_path.exists(), "blob file should be deleted");
    }

    /// Verify that empty shard directories are cleaned up bottom-up after sweep.
    #[test]
    fn sweep_cleans_empty_shard_directories() {
        use std::collections::HashSet;
        use std::fs;

        let tmp = tempfile::tempdir().unwrap();
        let cas_root = tmp.path().join("cas");

        // Create a single object via shard_path
        let hash = lillux::sha256_hex(b"lonely-object");
        let path = lillux::shard_path(&cas_root, "objects", &hash, ".json");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"{}").unwrap();

        // Verify shard dirs exist
        let shard2 = path.parent().unwrap();
        let shard1 = shard2.parent().unwrap();
        assert!(shard1.is_dir(), "shard1 dir should exist before sweep");
        assert!(shard2.is_dir(), "shard2 dir should exist before sweep");

        // Sweep with empty reachable — deletes the file and cleans dirs
        let reachable: HashSet<String> = HashSet::new();
        let mut result = GcResult::default();
        sweep_sharded_dir(
            &cas_root,
            "objects",
            ".json",
            &reachable,
            false,
            &mut result,
        )
        .unwrap();

        assert_eq!(result.deleted_objects, 1);
        assert!(!path.exists(), "file should be gone");
        assert!(!shard2.exists(), "shard2 dir should be cleaned up");
        assert!(!shard1.exists(), "shard1 dir should be cleaned up");
    }

    #[test]
    fn sweep_reclaims_exact_atomic_publication_staging_files() {
        use std::collections::HashSet;
        use std::fs;

        let tmp = tempfile::tempdir().unwrap();
        let cas_root = tmp.path().join("cas");
        let object_hash = lillux::sha256_hex(b"staged-object");
        let object_path = lillux::shard_path(&cas_root, "objects", &object_hash, ".json");
        let object_temp = object_path
            .parent()
            .unwrap()
            .join(format!(".{object_hash}.json.tmp.123.4"));
        fs::create_dir_all(object_temp.parent().unwrap()).unwrap();
        fs::write(&object_temp, b"partial object").unwrap();

        let blob_hash = lillux::sha256_hex(b"staged-blob");
        let blob_path = lillux::shard_path(&cas_root, "blobs", &blob_hash, "");
        let blob_temp = blob_path
            .parent()
            .unwrap()
            .join(format!(".{blob_hash}.tmp.456.7"));
        fs::create_dir_all(blob_temp.parent().unwrap()).unwrap();
        fs::write(&blob_temp, b"partial blob").unwrap();

        let mut result = GcResult::default();
        sweep_sharded_dir(
            &cas_root,
            "objects",
            ".json",
            &HashSet::new(),
            false,
            &mut result,
        )
        .unwrap();
        sweep_sharded_dir(&cas_root, "blobs", "", &HashSet::new(), false, &mut result).unwrap();

        assert_eq!(result.deleted_cas_staging_files, 2);
        assert!(!object_temp.exists());
        assert!(!blob_temp.exists());
    }

    #[test]
    fn sweep_rejects_noncanonical_atomic_staging_names() {
        use std::collections::HashSet;
        use std::fs;

        let tmp = tempfile::tempdir().unwrap();
        let cas_root = tmp.path().join("cas");
        let hash = lillux::sha256_hex(b"malformed-stage");
        let path = lillux::shard_path(&cas_root, "objects", &hash, ".json");
        let malformed = path
            .parent()
            .unwrap()
            .join(format!(".{hash}.json.tmp.not-a-pid.1"));
        fs::create_dir_all(malformed.parent().unwrap()).unwrap();
        fs::write(&malformed, b"partial").unwrap();

        let error = sweep_sharded_dir(
            &cas_root,
            "objects",
            ".json",
            &HashSet::new(),
            false,
            &mut GcResult::default(),
        )
        .unwrap_err();

        assert!(error.to_string().contains("unexpected CAS object filename"));
        assert!(malformed.is_file());
    }

    #[test]
    fn deep_runtime_purge_removes_only_runtime_disposable_paths() {
        use std::fs;

        let tmp = tempfile::tempdir().unwrap();
        let runtime_state_dir = tmp.path().join("state");
        let chain_ref = runtime_state_dir.join("refs/generic/chains/T-1/head");
        let project_ref = runtime_state_dir.join("refs/projects/fp/project/head");
        let bundle_event_ref =
            runtime_state_dir.join("refs/generic/bundle_events/email/event/head");
        let cache_file = runtime_state_dir.join("cache/executors/hash/runtime/bin");
        let execution_file = runtime_state_dir.join("cache/executions/live/project.yaml");
        let verified_code_file = runtime_state_dir.join("cache/verified-code/generation/digest");
        let trace_file = runtime_state_dir.join("trace-events.ndjson");
        let thread_meta = runtime_state_dir.join("threads/T-1/meta.json");
        let thread_checkpoint = runtime_state_dir.join("threads/T-1/checkpoints/ck.bin");

        fs::create_dir_all(chain_ref.parent().unwrap()).unwrap();
        fs::create_dir_all(project_ref.parent().unwrap()).unwrap();
        fs::create_dir_all(bundle_event_ref.parent().unwrap()).unwrap();
        fs::create_dir_all(cache_file.parent().unwrap()).unwrap();
        fs::create_dir_all(execution_file.parent().unwrap()).unwrap();
        fs::create_dir_all(verified_code_file.parent().unwrap()).unwrap();
        fs::create_dir_all(thread_checkpoint.parent().unwrap()).unwrap();
        fs::write(&chain_ref, b"chain").unwrap();
        fs::write(&project_ref, b"project").unwrap();
        fs::write(&bundle_event_ref, b"bundle-event").unwrap();
        fs::write(&cache_file, b"cache").unwrap();
        fs::write(&execution_file, b"active workspace").unwrap();
        fs::write(&verified_code_file, b"verified bytes").unwrap();
        fs::write(&trace_file, b"trace line\n").unwrap();
        fs::write(&thread_meta, b"meta").unwrap();
        fs::write(&thread_checkpoint, b"ckpt").unwrap();

        let mut result = GcResult::default();
        let params = GcParams {
            deep: true,
            ..GcParams::default()
        };
        let fire_retention = retention::FireRetentionPolicy::from_bounds(
            params.schedule_fire_max_age_days,
            params.schedule_fire_max_count,
        );
        purge_runtime_state(&runtime_state_dir, &params, fire_retention, &mut result).unwrap();

        assert!(
            chain_ref.exists(),
            "deep purge must not remove chain refs without daemon-owned policy and liveness checks"
        );
        assert!(
            !cache_file.exists(),
            "deep purge should drop executor cache files"
        );
        assert!(
            execution_file.exists(),
            "deep purge must not unlink request-owned execution workspaces"
        );
        assert!(
            verified_code_file.exists(),
            "deep purge must not unlink active verified-code generations"
        );
        assert_eq!(
            fs::metadata(&trace_file).unwrap().len(),
            0,
            "deep purge should truncate daemon trace output"
        );
        assert!(
            thread_meta.exists(),
            "deep purge must NOT blindly drop per-thread audit metadata — there is no liveness guard at this layer"
        );
        assert!(
            thread_checkpoint.exists(),
            "deep purge must NOT drop per-thread state — real checkpoints live under <app_root>/threads and need a liveness-guarded GC"
        );
        assert!(
            project_ref.exists(),
            "deep purge must preserve project refs"
        );
        assert!(
            bundle_event_ref.exists(),
            "deep purge must preserve bundle/application event refs"
        );
        assert!(result.deleted_runtime_files >= 2);
        assert!(result.freed_bytes >= b"cache".len() as u64);
    }

    #[test]
    fn gc_params_fields_default_when_omitted() {
        // The service schema advertises dry_run/compact as optional (boolean?);
        // omitting them must deserialize to false rather than erroring, so the
        // bare `ryeos maintenance gc` command works.
        let p: GcParams = serde_json::from_value(serde_json::json!({})).unwrap();
        assert!(!p.dry_run);
        assert!(!p.compact);
        assert!(!p.deep);
        assert!(!p.purge_cache);
        assert!(!p.truncate_trace);
        assert!(!p.prune_runtime_history);
        assert!(p.schedule_fire_max_age_days.is_none());
        assert!(p.schedule_fire_max_count.is_none());
        assert!(p.sync_job_retention_days.is_none());
        assert!(p.seat_lease_grace_seconds.is_none());
        assert!(p.policy.is_none());

        let p: GcParams = serde_json::from_value(serde_json::json!({"dry_run": true})).unwrap();
        assert!(p.dry_run);
        assert!(!p.compact);

        let p: GcParams = serde_json::from_value(serde_json::json!({"compact": true})).unwrap();
        assert!(!p.dry_run);
        assert!(p.compact);
        assert!(p.validate().is_err());

        let p: GcParams = serde_json::from_value(serde_json::json!({
            "compact": true,
            "policy": {"manual_pushes": 10, "auto_snapshots": 30}
        }))
        .unwrap();
        p.validate().unwrap();

        let p: GcParams = serde_json::from_value(serde_json::json!({"deep": true})).unwrap();
        assert!(p.deep);
    }
}
