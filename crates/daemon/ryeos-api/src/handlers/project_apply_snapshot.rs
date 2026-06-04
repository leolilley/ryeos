//! `project/apply-snapshot` — apply an AI-only snapshot to a live project.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::{anyhow, Context, Result};
use lillux::cas::CasStore;
use serde::Deserialize;
use serde_json::Value;

use crate::handler_context::HandlerContext;
use crate::registry::ServiceDescriptor;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;
use ryeos_state::objects::{ItemSource, ProjectSnapshot, SourceManifest};
use ryeos_state::project_sync::ProjectSyncScope;

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub project_path: String,
    pub snapshot_hash: String,
    #[serde(default)]
    pub expected_deployed_hash: Option<String>,
    #[serde(default)]
    pub force: bool,
}

pub async fn handle(req: Request, ctx: HandlerContext, state: Arc<AppState>) -> Result<Value> {
    ctx.require_verified().map_err(|e| anyhow!(e))?;

    let project_path = canonical_existing_project_path(&req.project_path)?;
    let canonical_project_path = project_path.to_string_lossy().to_string();
    let project_hash = ryeos_state::refs::deployed_project_key(&canonical_project_path);
    let apply_lock = project_apply_lock(&project_hash);
    let _apply_guard = apply_lock.lock().map_err(|_| {
        anyhow!(
            "project apply lock poisoned for '{}'",
            canonical_project_path
        )
    })?;
    let _permit = state
        .write_barrier
        .try_acquire()
        .map_err(|e| anyhow!("cannot acquire CAS write permit: {e}"))?;

    let cas_root = state.state_store.cas_root()?;
    let refs_root = state.state_store.refs_root()?;
    let cas = CasStore::new(cas_root);

    let principal_key = ryeos_state::refs::principal_storage_key(&ctx.fingerprint);
    let pushed_head =
        ryeos_state::refs::read_project_head_ref(&refs_root, principal_key, &project_hash)?;
    if pushed_head.as_deref() != Some(req.snapshot_hash.as_str()) {
        anyhow::bail!(
            "project.apply-snapshot refused: caller's staged HEAD for project '{}' is {:?}, not requested snapshot {}",
            canonical_project_path,
            pushed_head,
            req.snapshot_hash
        );
    }

    let snapshot_obj = cas
        .get_object(&req.snapshot_hash)?
        .ok_or_else(|| anyhow!("snapshot {} not found in CAS", req.snapshot_hash))?;
    let snapshot = ProjectSnapshot::from_value(&snapshot_obj)?;
    if snapshot.project_sync_scope != ProjectSyncScope::AiOnly {
        anyhow::bail!(
            "project.apply-snapshot only accepts ai_only snapshots in v1 (got {:?})",
            snapshot.project_sync_scope
        );
    }
    if snapshot.user_manifest_hash.is_some() {
        anyhow::bail!("project.apply-snapshot refuses snapshots with user_manifest_hash");
    }

    let manifest_obj = cas
        .get_object(&snapshot.project_manifest_hash)?
        .ok_or_else(|| {
            anyhow!(
                "manifest {} not found in CAS",
                snapshot.project_manifest_hash
            )
        })?;
    let manifest = SourceManifest::from_value(&manifest_obj)?;
    ryeos_state::project_sync::validate_project_manifest_paths(
        &manifest,
        ProjectSyncScope::AiOnly,
    )?;

    let current_ref = ryeos_state::refs::read_deployed_project_ref(&refs_root, &project_hash)?;
    let previous_deployed_hash = current_ref.as_ref().map(|r| r.target_hash.clone());
    if !req.force {
        if let Some(expected) = req.expected_deployed_hash.as_deref() {
            if expected.is_empty() {
                if previous_deployed_hash.is_some() {
                    anyhow::bail!(
                        "deployed project conflict for '{}': expected no deployed snapshot, got {:?}",
                        canonical_project_path,
                        previous_deployed_hash
                    );
                }
            } else if previous_deployed_hash.as_deref() != Some(expected) {
                anyhow::bail!(
                    "deployed project conflict for '{}': expected {}, got {:?}",
                    canonical_project_path,
                    expected,
                    previous_deployed_hash
                );
            }
        }
    }

    let staging_root = unique_staging_root(&project_path);
    if staging_root.exists() {
        fs::remove_dir_all(&staging_root)
            .with_context(|| format!("remove stale staging dir {}", staging_root.display()))?;
    }
    fs::create_dir_all(&staging_root)
        .with_context(|| format!("create staging dir {}", staging_root.display()))?;

    let apply_result = (|| -> Result<ApplyReport> {
        materialize_manifest_to_staging(&cas, &manifest, &staging_root)?;
        preflight_project_deploy(&staging_root, &project_path)?;
        replace_managed_roots(&project_path, &staging_root)
    })();
    let cleanup = fs::remove_dir_all(&staging_root);
    if let Err(err) = cleanup {
        tracing::warn!(path = %staging_root.display(), error = %err, "failed to remove project apply staging dir");
    }
    let report = apply_result?;

    let signer = ryeos_app::state_store::NodeIdentitySigner::from_identity(&state.identity);
    if let Some(ref current) = previous_deployed_hash {
        ryeos_state::refs::advance_deployed_project_ref(
            &refs_root,
            &project_hash,
            &req.snapshot_hash,
            current,
            &signer,
        )?;
    } else {
        ryeos_state::refs::write_deployed_project_ref(
            &refs_root,
            &project_hash,
            &req.snapshot_hash,
            &signer,
        )?;
    }

    Ok(serde_json::json!({
        "project_path": canonical_project_path,
        "project_hash": project_hash,
        "snapshot_hash": req.snapshot_hash,
        "previous_deployed_hash": previous_deployed_hash,
        "project_sync_scope": snapshot.project_sync_scope,
        "manifest_entries": manifest.item_source_hashes.len(),
        "files_materialized": report.files_materialized,
        "roots_replaced": report.roots_replaced,
        "roots_deleted": report.roots_deleted,
    }))
}

pub(crate) fn project_apply_lock(project_hash: &str) -> Arc<Mutex<()>> {
    static LOCKS: OnceLock<Mutex<HashMap<String, Arc<Mutex<()>>>>> = OnceLock::new();
    let locks = LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut locks = locks.lock().unwrap_or_else(|e| e.into_inner());
    locks
        .entry(project_hash.to_string())
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone()
}

fn canonical_existing_project_path(project_path: &str) -> Result<PathBuf> {
    let path = Path::new(project_path);
    if !path.is_absolute() {
        anyhow::bail!("project_path '{}' is not absolute", project_path);
    }
    let canonical = path.canonicalize().with_context(|| {
        format!(
            "cannot canonicalize project_path '{}'; ensure it exists",
            project_path
        )
    })?;
    if !canonical.is_dir() {
        anyhow::bail!("project_path '{}' is not a directory", canonical.display());
    }
    Ok(canonical)
}

fn unique_staging_root(project_path: &Path) -> PathBuf {
    let nonce = rand::Rng::gen::<u64>(&mut rand::thread_rng());
    project_path.join(format!(".ryeos-ai-sync-staging-{nonce:016x}"))
}

fn materialize_manifest_to_staging(
    cas: &CasStore,
    manifest: &SourceManifest,
    staging_root: &Path,
) -> Result<usize> {
    let mut count = 0usize;
    for (rel_path, item_hash) in &manifest.item_source_hashes {
        ryeos_state::project_sync::validate_project_manifest_path(
            rel_path,
            ProjectSyncScope::AiOnly,
        )?;
        let item_obj = cas.get_object(item_hash)?.ok_or_else(|| {
            anyhow!(
                "item source object {} for '{}' not found",
                item_hash,
                rel_path
            )
        })?;
        let item = ItemSource::from_value(&item_obj)?;
        let blob = cas.get_blob(&item.content_blob_hash)?.ok_or_else(|| {
            anyhow!(
                "blob {} for '{}' not found",
                item.content_blob_hash,
                rel_path
            )
        })?;
        if lillux::cas::sha256_hex(&blob) != item.integrity {
            anyhow::bail!("integrity mismatch for '{}'", rel_path);
        }

        let target = staging_root.join(rel_path);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        lillux::cas::atomic_write(&target, &blob)?;
        apply_mode(&target, item.mode)?;
        count += 1;
    }
    Ok(count)
}

fn apply_mode(path: &Path, mode: Option<u32>) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = mode.unwrap_or(0o644) & 0o777;
        fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ScheduleDeclarationFile {
    category: String,
    version: String,
    schema_version: String,
    #[serde(default)]
    #[serde(rename = "description")]
    _description: Option<String>,
    schedules: Vec<ScheduleDeclaration>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ScheduleDeclaration {
    schedule_id: String,
    item_ref: String,
    schedule_type: String,
    expression: String,
    #[serde(default)]
    timezone: Option<String>,
    #[serde(default)]
    misfire_policy: Option<String>,
    #[serde(default)]
    overlap_policy: Option<String>,
    #[serde(default)]
    lateness_grace_secs: Option<i64>,
    #[serde(default = "default_schedule_enabled")]
    enabled: bool,
    #[serde(default)]
    project_root: Option<String>,
    #[serde(default)]
    params: Value,
}

fn default_schedule_enabled() -> bool {
    true
}

fn preflight_project_deploy(staging_root: &Path, project_path: &Path) -> Result<()> {
    preflight_schedule_declarations(staging_root, project_path)?;
    Ok(())
}

fn preflight_schedule_declarations(staging_root: &Path, project_path: &Path) -> Result<usize> {
    let schedules_root = staging_root.join(".ai/config/schedules");
    if !schedules_root.is_dir() {
        return Ok(0);
    }

    let canonical_project_path = project_path.to_string_lossy().to_string();
    let mut seen = HashSet::new();
    let mut count = 0usize;
    for path in schedule_declaration_files(&schedules_root)? {
        let rel_path = path
            .strip_prefix(staging_root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        let content = fs::read_to_string(&path)
            .with_context(|| format!("read schedule declaration {}", rel_path))?;
        let body = lillux::signature::strip_signature_lines(&content);
        let file: ScheduleDeclarationFile = serde_yaml::from_str(&body)
            .with_context(|| format!("parse schedule declaration {}", rel_path))?;
        validate_schedule_declaration_file(&file, &rel_path)?;
        for schedule in &file.schedules {
            validate_schedule_declaration(schedule, &rel_path, &canonical_project_path)?;
            if !seen.insert(schedule.schedule_id.clone()) {
                anyhow::bail!(
                    "duplicate schedule_id '{}' across project schedule declarations",
                    schedule.schedule_id
                );
            }
            count += 1;
        }
    }

    Ok(count)
}

fn schedule_declaration_files(root: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    collect_schedule_declaration_files(root, &mut files)?;
    files.sort();
    Ok(files)
}

fn collect_schedule_declaration_files(dir: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    for entry in fs::read_dir(dir).with_context(|| format!("read {}", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        let ft = entry.file_type()?;
        if ft.is_symlink() {
            anyhow::bail!(
                "schedule declaration path '{}' is a symlink; refusing project deploy",
                path.display()
            );
        }
        if ft.is_dir() {
            collect_schedule_declaration_files(&path, files)?;
        } else if ft.is_file() {
            let ext = path.extension().and_then(|ext| ext.to_str());
            if matches!(ext, Some("yaml" | "yml")) {
                files.push(path);
            }
        }
    }
    Ok(())
}

fn validate_schedule_declaration_file(
    file: &ScheduleDeclarationFile,
    rel_path: &str,
) -> Result<()> {
    if file.category != "schedules" {
        anyhow::bail!(
            "schedule declaration '{}' has category '{}', expected 'schedules'",
            rel_path,
            file.category
        );
    }
    if file.version != "1.0.0" {
        anyhow::bail!(
            "schedule declaration '{}' has unsupported version '{}', expected '1.0.0'",
            rel_path,
            file.version
        );
    }
    if file.schema_version != "1.0.0" {
        anyhow::bail!(
            "schedule declaration '{}' has unsupported schema_version '{}', expected '1.0.0'",
            rel_path,
            file.schema_version
        );
    }
    if file.schedules.is_empty() {
        anyhow::bail!("schedule declaration '{}' contains no schedules", rel_path);
    }
    Ok(())
}

fn validate_schedule_declaration(
    schedule: &ScheduleDeclaration,
    rel_path: &str,
    canonical_project_path: &str,
) -> Result<()> {
    ryeos_scheduler::crontab::validate_schedule_id(&schedule.schedule_id)
        .with_context(|| format!("invalid schedule_id in {}", rel_path))?;
    ryeos_engine::canonical_ref::CanonicalRef::parse(&schedule.item_ref)
        .with_context(|| format!("invalid item_ref for schedule '{}'", schedule.schedule_id))?;
    ryeos_scheduler::crontab::validate_expression(&schedule.schedule_type, &schedule.expression)
        .with_context(|| format!("invalid expression for schedule '{}'", schedule.schedule_id))?;
    let timezone = schedule.timezone.as_deref().unwrap_or("UTC");
    ryeos_scheduler::crontab::validate_timezone(timezone)
        .with_context(|| format!("invalid timezone for schedule '{}'", schedule.schedule_id))?;
    if let Some(ref p) = schedule.overlap_policy {
        if !matches!(p.as_str(), "allow" | "skip" | "cancel_previous") {
            anyhow::bail!(
                "invalid overlap_policy '{}' for schedule '{}'",
                p,
                schedule.schedule_id
            );
        }
    }
    if let Some(ref p) = schedule.misfire_policy {
        if !is_valid_misfire_policy(p) {
            anyhow::bail!(
                "invalid misfire_policy '{}' for schedule '{}'",
                p,
                schedule.schedule_id
            );
        }
    }
    if let Some(secs) = schedule.lateness_grace_secs {
        if secs <= 0 {
            anyhow::bail!(
                "lateness_grace_secs must be positive for schedule '{}'",
                schedule.schedule_id
            );
        }
    }
    if !schedule.params.is_null() && !schedule.params.is_object() {
        anyhow::bail!(
            "params must be a mapping for schedule '{}'",
            schedule.schedule_id
        );
    }
    let _enabled = schedule.enabled;
    if let Some(ref project_root) = schedule.project_root {
        let declared = Path::new(project_root);
        if !declared.is_absolute() {
            anyhow::bail!(
                "project_root for schedule '{}' must be absolute",
                schedule.schedule_id
            );
        }
        if project_root != canonical_project_path {
            anyhow::bail!(
                "project_root for schedule '{}' is '{}', expected '{}'; project schedule declarations cannot target another project",
                schedule.schedule_id,
                project_root,
                canonical_project_path
            );
        }
    }
    Ok(())
}

fn is_valid_misfire_policy(p: &str) -> bool {
    match p {
        "skip" | "fire_once_now" => true,
        s if s.starts_with("catch_up_bounded:") => s
            .strip_prefix("catch_up_bounded:")
            .and_then(|n| n.parse::<usize>().ok())
            .is_some(),
        s if s.starts_with("catch_up_within_secs:") => s
            .strip_prefix("catch_up_within_secs:")
            .and_then(|n| n.parse::<u64>().ok())
            .is_some(),
        _ => false,
    }
}

#[derive(Debug, Default)]
struct ApplyReport {
    files_materialized: usize,
    roots_replaced: usize,
    roots_deleted: usize,
}

#[derive(Debug)]
struct RootSwap {
    dest: PathBuf,
    backup: Option<PathBuf>,
    installed: bool,
}

fn replace_managed_roots(project_path: &Path, staging_root: &Path) -> Result<ApplyReport> {
    let mut swaps: Vec<RootSwap> = Vec::new();
    let mut report = ApplyReport::default();
    report.files_materialized = count_files(staging_root)?;

    let result = (|| -> Result<()> {
        for rel_root in ryeos_state::project_sync::materialized_project_ai_surface_roots() {
            reject_symlinked_existing_path(project_path, rel_root)?;
            let dest = project_path.join(rel_root);
            let staged = staging_root.join(rel_root);
            let staged_exists = staged.exists();
            let backup = if dest.exists() {
                let backup = dest.with_file_name(format!(
                    ".{}.ryeos-backup-{:016x}",
                    dest.file_name().and_then(|n| n.to_str()).unwrap_or("root"),
                    rand::Rng::gen::<u64>(&mut rand::thread_rng())
                ));
                fs::rename(&dest, &backup)
                    .with_context(|| format!("backup managed root {}", dest.display()))?;
                Some(backup)
            } else {
                None
            };

            if staged_exists {
                if let Some(parent) = dest.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::rename(&staged, &dest)
                    .with_context(|| format!("install managed root {}", dest.display()))?;
                report.roots_replaced += 1;
                swaps.push(RootSwap {
                    dest,
                    backup,
                    installed: true,
                });
            } else {
                report.roots_deleted += usize::from(backup.is_some());
                swaps.push(RootSwap {
                    dest,
                    backup,
                    installed: false,
                });
            }
        }
        Ok(())
    })();

    if let Err(err) = result {
        rollback_swaps(&swaps);
        return Err(err);
    }

    for swap in swaps {
        if let Some(backup) = swap.backup {
            let _ = remove_path_any(&backup);
        }
    }
    Ok(report)
}

fn rollback_swaps(swaps: &[RootSwap]) {
    for swap in swaps.iter().rev() {
        if swap.installed && swap.dest.exists() {
            let _ = remove_path_any(&swap.dest);
        }
        if let Some(backup) = &swap.backup {
            if backup.exists() {
                let _ = fs::rename(backup, &swap.dest);
            }
        }
    }
}

fn remove_path_any(path: &Path) -> std::io::Result<()> {
    let md = fs::symlink_metadata(path)?;
    if md.is_dir() && !md.file_type().is_symlink() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
}

fn reject_symlinked_existing_path(project_path: &Path, rel_root: &str) -> Result<()> {
    let mut current = project_path.to_path_buf();
    for component in rel_root.split('/') {
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(md) if md.file_type().is_symlink() => {
                anyhow::bail!(
                    "managed root path '{}' contains symlink '{}'; refusing apply",
                    rel_root,
                    current.display()
                );
            }
            Ok(_) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => break,
            Err(err) => return Err(err).with_context(|| format!("inspect {}", current.display())),
        }
    }
    Ok(())
}

fn count_files(root: &Path) -> Result<usize> {
    if !root.exists() {
        return Ok(0);
    }
    let mut count = 0usize;
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let ft = entry.file_type()?;
        if ft.is_dir() {
            count += count_files(&entry.path())?;
        } else if ft.is_file() {
            count += 1;
        }
    }
    Ok(count)
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:project/apply-snapshot",
    endpoint: "project.apply-snapshot",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.project.apply"],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: Request = crate::handler_error::parse_request(params)?;
            handle(req, ctx, state).await
        })
    },
};

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use tempfile::TempDir;

    #[test]
    fn replace_managed_roots_deletes_missing_and_preserves_app_files() {
        let project = TempDir::new().unwrap();
        std::fs::create_dir_all(project.path().join(".ai/directives")).unwrap();
        std::fs::create_dir_all(project.path().join(".ai/tools")).unwrap();
        std::fs::create_dir_all(project.path().join(".ai/config/schedules")).unwrap();
        std::fs::create_dir_all(project.path().join(".ai/node/schedules")).unwrap();
        std::fs::create_dir_all(project.path().join("src")).unwrap();
        std::fs::write(project.path().join(".ai/directives/old.md"), "old").unwrap();
        std::fs::write(project.path().join(".ai/tools/old.sh"), "old").unwrap();
        std::fs::write(project.path().join(".ai/config/schedules/old.yaml"), "old").unwrap();
        std::fs::write(
            project.path().join(".ai/node/schedules/runtime.yaml"),
            "runtime",
        )
        .unwrap();
        std::fs::write(project.path().join("src/index.ts"), "app").unwrap();

        let staging = TempDir::new().unwrap();
        std::fs::create_dir_all(staging.path().join(".ai/directives")).unwrap();
        std::fs::create_dir_all(staging.path().join(".ai/config/schedules")).unwrap();
        std::fs::write(staging.path().join(".ai/directives/new.md"), "new").unwrap();
        std::fs::write(staging.path().join(".ai/config/schedules/new.yaml"), "new").unwrap();

        let report = replace_managed_roots(project.path(), staging.path()).unwrap();
        assert!(report.roots_replaced >= 1);
        assert!(report.roots_deleted >= 1);
        assert!(project.path().join(".ai/directives/new.md").exists());
        assert!(!project.path().join(".ai/directives/old.md").exists());
        assert!(!project.path().join(".ai/tools").exists());
        assert!(project
            .path()
            .join(".ai/config/schedules/new.yaml")
            .exists());
        assert!(!project
            .path()
            .join(".ai/config/schedules/old.yaml")
            .exists());
        assert_eq!(
            std::fs::read_to_string(project.path().join(".ai/node/schedules/runtime.yaml"))
                .unwrap(),
            "runtime"
        );
        assert_eq!(
            std::fs::read_to_string(project.path().join("src/index.ts")).unwrap(),
            "app"
        );
    }

    #[test]
    fn replace_managed_roots_rejects_symlinked_live_root() {
        #[cfg(not(unix))]
        {
            return;
        }

        let project = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        std::fs::create_dir_all(project.path().join(".ai")).unwrap();
        std::os::unix::fs::symlink(outside.path(), project.path().join(".ai/directives")).unwrap();

        let staging = TempDir::new().unwrap();
        std::fs::create_dir_all(staging.path().join(".ai/directives")).unwrap();
        std::fs::write(staging.path().join(".ai/directives/new.md"), "new").unwrap();

        let err = replace_managed_roots(project.path(), staging.path())
            .expect_err("symlink root must be rejected");
        assert!(format!("{err:#}").contains("symlink"));
    }

    #[test]
    fn materialize_manifest_restores_executable_mode() {
        #[cfg(not(unix))]
        {
            return;
        }

        use std::os::unix::fs::PermissionsExt;

        let cas_dir = TempDir::new().unwrap();
        let cas = CasStore::new(cas_dir.path().to_path_buf());
        let bytes = b"#!/bin/sh\n";
        let blob_hash = cas.store_blob(bytes).unwrap();
        let item = ItemSource {
            item_ref: ".ai/tools/run.sh".into(),
            content_blob_hash: blob_hash,
            integrity: lillux::cas::sha256_hex(bytes),
            signature_info: None,
            mode: Some(0o755),
        };
        let item_hash = cas.store_object(&item.to_value()).unwrap();
        let mut map = HashMap::new();
        map.insert(".ai/tools/run.sh".to_string(), item_hash);
        let manifest = SourceManifest {
            item_source_hashes: map,
        };
        let staging = TempDir::new().unwrap();

        materialize_manifest_to_staging(&cas, &manifest, staging.path()).unwrap();
        let mode = std::fs::metadata(staging.path().join(".ai/tools/run.sh"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o755);
    }

    #[test]
    fn apply_mode_clamps_special_bits() {
        #[cfg(not(unix))]
        {
            return;
        }

        use std::os::unix::fs::PermissionsExt;

        let dir = TempDir::new().unwrap();
        let path = dir.path().join("file");
        std::fs::write(&path, "content").unwrap();

        apply_mode(&path, Some(0o4755)).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o7777;
        assert_eq!(mode, 0o755);
    }

    #[test]
    fn preflight_schedule_declarations_accepts_valid_project_intent() {
        let project = TempDir::new().unwrap();
        let staging = TempDir::new().unwrap();
        let schedules = staging.path().join(".ai/config/schedules");
        std::fs::create_dir_all(&schedules).unwrap();
        std::fs::write(
            schedules.join("snap-track.yaml"),
            format!(
                r#"category: schedules
version: 1.0.0
schema_version: 1.0.0
schedules:
  - schedule_id: snap-track-discover-feed-scrape
    item_ref: graph:snap-track/discover_feed_scrape
    schedule_type: cron
    expression: "0 */15 * * * *"
    timezone: UTC
    misfire_policy: skip
    overlap_policy: skip
    enabled: true
    project_root: {}
    params:
      country: US
"#,
                project.path().display()
            ),
        )
        .unwrap();

        let count = preflight_schedule_declarations(staging.path(), project.path()).unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn preflight_schedule_declarations_rejects_duplicate_ids() {
        let project = TempDir::new().unwrap();
        let staging = TempDir::new().unwrap();
        let schedules = staging.path().join(".ai/config/schedules");
        std::fs::create_dir_all(&schedules).unwrap();
        for file in ["a.yaml", "b.yaml"] {
            std::fs::write(
                schedules.join(file),
                r#"category: schedules
version: 1.0.0
schema_version: 1.0.0
schedules:
  - schedule_id: duplicate-schedule
    item_ref: graph:snap-track/discover_feed_scrape
    schedule_type: cron
    expression: "0 */15 * * * *"
"#,
            )
            .unwrap();
        }

        let err = preflight_schedule_declarations(staging.path(), project.path())
            .expect_err("duplicate schedule ids must fail preflight");
        assert!(format!("{err:#}").contains("duplicate schedule_id"));
    }

    #[test]
    fn preflight_schedule_declarations_rejects_node_owned_fields() {
        let project = TempDir::new().unwrap();
        let staging = TempDir::new().unwrap();
        let schedules = staging.path().join(".ai/config/schedules");
        std::fs::create_dir_all(&schedules).unwrap();
        std::fs::write(
            schedules.join("bad.yaml"),
            r#"category: schedules
version: 1.0.0
schema_version: 1.0.0
schedules:
  - schedule_id: bad-schedule
    item_ref: graph:snap-track/discover_feed_scrape
    schedule_type: cron
    expression: "0 */15 * * * *"
    execution:
      requester_fingerprint: fp:test
"#,
        )
        .unwrap();

        let err = preflight_schedule_declarations(staging.path(), project.path())
            .expect_err("node-owned execution field must fail preflight");
        assert!(format!("{err:#}").contains("unknown field `execution`"));
    }

    #[test]
    fn preflight_schedule_declarations_rejects_other_project_root() {
        let project = TempDir::new().unwrap();
        let other_project = TempDir::new().unwrap();
        let staging = TempDir::new().unwrap();
        let schedules = staging.path().join(".ai/config/schedules");
        std::fs::create_dir_all(&schedules).unwrap();
        std::fs::write(
            schedules.join("bad-root.yaml"),
            format!(
                r#"category: schedules
version: 1.0.0
schema_version: 1.0.0
schedules:
  - schedule_id: wrong-root
    item_ref: graph:snap-track/discover_feed_scrape
    schedule_type: cron
    expression: "0 */15 * * * *"
    project_root: {}
"#,
                other_project.path().display()
            ),
        )
        .unwrap();

        let err = preflight_schedule_declarations(staging.path(), project.path())
            .expect_err("foreign project root must fail preflight");
        assert!(format!("{err:#}").contains("cannot target another project"));
    }
}
