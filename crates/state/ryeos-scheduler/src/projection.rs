//! Projection: sync between CAS files and `scheduler.sqlite3`.
//!
//! Schedule specs live as signed YAML in `.ai/node/schedules/`.
//! Fire history lives as JSONL in `.ai/state/schedules/*/fires.jsonl`.
//! The projection DB indexes both. Nuke the DB → rebuild from these files.

#[cfg(test)]
use std::fs;
use std::io::{BufRead, Read, Seek, SeekFrom, Write};
use std::path::Path;

use anyhow::{bail, Context, Result};

use super::db::SchedulerDb;
use super::types::{FireRecord, ScheduleSourceRecord, ScheduleSpecRecord};

/// A schedule source that crossed the exact scheduler trust boundary.
#[derive(Debug, Clone)]
pub struct VerifiedScheduleSource {
    pub record: ScheduleSourceRecord,
    pub signer_fingerprint: String,
    pub spec_hash: String,
}

impl VerifiedScheduleSource {
    pub fn to_spec_record(&self) -> Result<ScheduleSpecRecord> {
        self.record
            .to_spec_record(&self.signer_fingerprint, &self.spec_hash)
    }
}

/// Load the one current schedule source format.
///
/// Schedule authority is always a trusted, signed, regular `.yaml` file. No
/// caller can opt out of trust verification or reinterpret a generic YAML
/// mapping after verification.
pub fn load_verified_schedule_source(
    path: &Path,
    trust_store: &ryeos_engine::trust::TrustStore,
) -> Result<VerifiedScheduleSource> {
    let content = lillux::read_regular_file_to_string_no_follow(path)
        .with_context(|| format!("securely read schedule source {}", path.display()))?;
    verify_schedule_source_content(path, &content, trust_store)
}

/// Verify schedule bytes already read from an inode pinned by the caller. This
/// prevents read/verify/mutate workflows from reopening a pathname between the
/// trust decision and an inode-conditional replacement or deletion.
pub fn verify_schedule_source_content(
    path: &Path,
    content: &str,
    trust_store: &ryeos_engine::trust::TrustStore,
) -> Result<VerifiedScheduleSource> {
    let schedules_dir = path
        .parent()
        .context("schedule source has no schedules directory")?;
    let node_dir = schedules_dir
        .parent()
        .context("schedule source has no node directory")?;
    let ai_dir = node_dir
        .parent()
        .context("schedule source has no .ai directory")?;
    if schedules_dir.file_name().and_then(|name| name.to_str()) != Some("schedules")
        || node_dir.file_name().and_then(|name| name.to_str()) != Some("node")
        || ai_dir.file_name().and_then(|name| name.to_str()) != Some(ryeos_engine::AI_DIR)
    {
        bail!("schedule source must live under .ai/node/schedules");
    }
    if path.extension().and_then(|extension| extension.to_str()) != Some("yaml") {
        bail!(
            "schedule source {} must use the canonical .yaml extension",
            path.display()
        );
    }
    let expected_id = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .context("schedule source filename must be UTF-8")?;
    super::crontab::validate_schedule_id(expected_id)?;

    let envelope = ryeos_engine::contracts::SignatureEnvelope {
        prefix: "#".to_owned(),
        suffix: None,
        after_shebang: false,
    };
    let header = ryeos_engine::item_resolution::parse_signature_header(content, &envelope)
        .with_context(|| format!("schedule source {} has no valid signature", path.display()))?;
    for (field, hash) in [
        ("signer fingerprint", header.signer_fingerprint.as_str()),
        ("content hash", header.content_hash.as_str()),
    ] {
        if !lillux::cas::valid_hash(hash) || hash.bytes().any(|byte| byte.is_ascii_uppercase()) {
            bail!(
                "schedule source {} has non-canonical {}",
                path.display(),
                field
            );
        }
    }
    let (trust_class, _) =
        ryeos_engine::trust::verify_item_signature(content, &header, &envelope, trust_store)
            .with_context(|| format!("verify schedule source {}", path.display()))?;
    if trust_class != ryeos_engine::contracts::TrustClass::Trusted {
        bail!(
            "schedule source {} signer {} is not trusted",
            path.display(),
            header.signer_fingerprint
        );
    }

    let body = lillux::signature::strip_signature_lines(content);
    let record: ScheduleSourceRecord = serde_yaml::from_str(&body)
        .with_context(|| format!("decode current schedule source {}", path.display()))?;
    record
        .validate(Some(expected_id))
        .with_context(|| format!("validate schedule source {}", path.display()))?;

    Ok(VerifiedScheduleSource {
        record,
        signer_fingerprint: header.signer_fingerprint,
        // The verified body hash is the semantic schedule identity. Signature
        // timestamp/envelope changes do not invalidate cursors or manufacture
        // a schedule-spec update when the authored body is unchanged.
        spec_hash: header.content_hash,
    })
}

/// Append one strict, canonical fire snapshot. A crash-truncated final line is
/// removed under the same interprocess lock before appending, so every retry
/// converges to a replayable journal.
pub fn append_jsonl_entry<T: serde::Serialize>(path: &Path, entry: &T) -> Result<()> {
    let entry: FireRecord = serde_json::from_value(serde_json::to_value(entry)?)
        .context("fire journal append requires the current FireRecord shape")?;
    ensure_fire_journal_parent(path)?;
    let lock = lillux::ExclusiveFileLock::acquire(path)?;
    append_fire_jsonl_entry_with_lock(&lock, path, &entry)
}

/// Append beneath an already-pinned schedule directory.
pub fn append_fire_jsonl_entry_in_directory<T: serde::Serialize>(
    schedule_directory: &lillux::PinnedDirectory,
    expected_schedule_id: &str,
    entry: &T,
) -> Result<()> {
    super::crontab::validate_schedule_id(expected_schedule_id)?;
    let entry: FireRecord = serde_json::from_value(serde_json::to_value(entry)?)
        .context("fire journal append requires the current FireRecord shape")?;
    if entry.schedule_id != expected_schedule_id {
        bail!(
            "scheduler fire {} belongs to {}, not {}",
            entry.fire_id,
            entry.schedule_id,
            expected_schedule_id
        );
    }
    let name = std::ffi::OsStr::new("fires.jsonl");
    let lock = lillux::ExclusiveFileLock::acquire_in(schedule_directory, name)?;
    let path = schedule_directory.path().join(name);
    append_fire_jsonl_entry_with_lock(&lock, &path, &entry)
}

fn append_fire_jsonl_entry_with_lock(
    lock: &lillux::ExclusiveFileLock,
    path: &Path,
    entry: &FireRecord,
) -> Result<()> {
    let mut file = lock
        .open_target_append_create()
        .with_context(|| format!("open fires journal {}", path.display()))?;

    let length = file.metadata()?.len();
    if length > 0 {
        file.seek(SeekFrom::End(-1))?;
        let mut last = [0u8; 1];
        file.read_exact(&mut last)?;
        if last[0] != b'\n' {
            file.seek(SeekFrom::Start(0))?;
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes)?;
            let repaired_length = bytes
                .iter()
                .rposition(|byte| *byte == b'\n')
                .map_or(0, |index| index + 1);
            file.set_len(u64::try_from(repaired_length)?)?;
            file.sync_data()?;
        }
    }

    let mut line = entry.canonical_json_line()?;
    line.push('\n');
    file.write_all(line.as_bytes())
        .with_context(|| format!("append fire snapshot to {}", path.display()))?;
    file.sync_data()
        .with_context(|| format!("sync fire snapshot at {}", path.display()))?;
    lock.sync_parent()?;
    Ok(())
}

fn ensure_fire_journal_parent(path: &Path) -> Result<()> {
    if path.file_name().and_then(|name| name.to_str()) != Some("fires.jsonl") {
        bail!("scheduler fire journal must be named fires.jsonl");
    }
    let schedule_dir = path
        .parent()
        .context("fire journal has no schedule directory")?;
    let schedule_id = schedule_dir
        .file_name()
        .and_then(|name| name.to_str())
        .context("fire journal schedule directory must be UTF-8")?;
    super::crontab::validate_schedule_id(schedule_id)?;
    let schedules_dir = schedule_dir
        .parent()
        .context("fire journal has no schedules directory")?;
    if schedules_dir.file_name().and_then(|name| name.to_str()) != Some("schedules") {
        bail!("fire journal must live under the scheduler schedules directory");
    }
    let state_dir = schedules_dir
        .parent()
        .context("fire journal has no state directory")?;
    if state_dir.file_name().and_then(|name| name.to_str()) != Some("state") {
        bail!("fire journal must live under .ai/state/schedules");
    }
    let ai_dir = state_dir
        .parent()
        .context("fire journal has no .ai directory")?;
    if ai_dir.file_name().and_then(|name| name.to_str()) != Some(ryeos_engine::AI_DIR) {
        bail!("fire journal must live under .ai/state/schedules");
    }

    Ok(())
}

fn runtime_state_dir(app_root: &Path) -> std::path::PathBuf {
    app_root.join(ryeos_engine::AI_DIR).join("state")
}

/// Commit one complete fire snapshot and synchronously drain the transactional
/// outbox into its durable JSONL journal. The caller must hold the scheduler
/// runtime gate for the full call.
pub fn persist_fire_snapshot(app_root: &Path, db: &SchedulerDb, record: &FireRecord) -> Result<()> {
    db.upsert_fire(record)?;
    db.drain_fire_outbox(&runtime_state_dir(app_root))?;
    Ok(())
}

/// Claim a fire exactly once and durably publish the claimed snapshot before
/// returning success. The caller must hold the scheduler runtime gate.
pub fn claim_fire_snapshot(app_root: &Path, db: &SchedulerDb, record: &FireRecord) -> Result<bool> {
    let claimed = db.claim_fire(record)?;
    db.drain_fire_outbox(&runtime_state_dir(app_root))?;
    Ok(claimed)
}

/// Prove an interrupted immutable dispatch record remains reclaimable.
pub fn reclaim_fire_snapshot(app_root: &Path, db: &SchedulerDb, fire_id: &str) -> Result<bool> {
    let reclaimed = db.reclaim_fire(fire_id)?;
    let _ = app_root;
    Ok(reclaimed)
}

/// Enumerate the one canonical schedule-source namespace. Unsupported files,
/// directories, symlinks, filenames, and locations are authority errors.
pub fn canonical_schedule_source_paths(schedules_dir: &Path) -> Result<Vec<std::path::PathBuf>> {
    let node_dir = schedules_dir
        .parent()
        .context("schedules directory has no node directory")?;
    let ai_dir = node_dir
        .parent()
        .context("schedules directory has no .ai directory")?;
    if schedules_dir.file_name().and_then(|name| name.to_str()) != Some("schedules")
        || node_dir.file_name().and_then(|name| name.to_str()) != Some("node")
        || ai_dir.file_name().and_then(|name| name.to_str()) != Some(ryeos_engine::AI_DIR)
    {
        bail!("schedule sources must live under .ai/node/schedules");
    }

    let paths = lillux::collect_regular_files_no_follow(schedules_dir, false)?.unwrap_or_default();
    paths
        .into_iter()
        .map(|path| {
            if path.extension().and_then(|extension| extension.to_str()) != Some("yaml") {
                bail!(
                    "schedule directory contains unsupported non-.yaml file {}",
                    path.display()
                );
            }
            let schedule_id = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .context("schedule source filename must be UTF-8")?;
            super::crontab::validate_schedule_id(schedule_id)?;
            Ok(path)
        })
        .collect()
}

/// Atomically rebuild `schedule_specs` from `.ai/node/schedules/*.yaml`.
/// Every source is verified and validated before the projection transaction
/// begins. Any malformed, untrusted, duplicate, or non-canonical source keeps
/// the previous complete projection intact.
pub fn rebuild_specs_from_dir(
    schedules_dir: &Path,
    db: &SchedulerDb,
    trust_store: &ryeos_engine::trust::TrustStore,
) -> Result<Vec<String>> {
    let mut seen = std::collections::HashSet::new();
    let mut records = Vec::new();
    for path in canonical_schedule_source_paths(schedules_dir)? {
        let verified = load_verified_schedule_source(&path, trust_store)?;
        if !seen.insert(verified.record.schedule_id.clone()) {
            bail!(
                "duplicate schedule source for {}",
                verified.record.schedule_id
            );
        }
        records.push(verified.to_spec_record()?);
    }

    let live_ids = records
        .iter()
        .map(|record| record.schedule_id.clone())
        .collect();
    db.replace_specs(&records)?;
    Ok(live_ids)
}

/// Rebuild `schedule_fires` from `.ai/state/schedules/*/fires.jsonl`.
pub fn rebuild_fires_from_dir(fires_dir: &Path, db: &SchedulerDb) -> Result<()> {
    db.begin_fire_projection_rebuild()?;

    let Some(tree) = lillux::collect_directory_tree_no_follow(fires_dir)? else {
        db.finish_fire_projection_rebuild()?;
        return Ok(());
    };
    for directory in &tree.directories {
        let relative = directory.strip_prefix(fires_dir)?;
        if relative.components().count() != 1 {
            bail!(
                "scheduler fires root contains unsupported nested directory {}",
                directory.display()
            );
        }
        let schedule_id = relative
            .to_str()
            .context("scheduler fire directory name must be UTF-8")?;
        super::crontab::validate_schedule_id(schedule_id)?;
    }
    for jsonl_path in tree.regular_files {
        let relative = jsonl_path.strip_prefix(fires_dir)?;
        let mut components = relative.components();
        let schedule_id = components
            .next()
            .and_then(|component| component.as_os_str().to_str())
            .context("scheduler fire directory name must be UTF-8")?;
        let filename = components
            .next()
            .and_then(|component| component.as_os_str().to_str());
        if components.next().is_some() {
            bail!(
                "scheduler fires root contains unsupported file {}",
                jsonl_path.display()
            );
        }
        // `ExclusiveFileLock` establishes this one durable sibling anchor on
        // the first append. It is part of the closed journal namespace, not a
        // fire record. Every other regular file remains an authority error.
        if filename == Some(".fires.jsonl.lock") {
            continue;
        }
        if filename != Some("fires.jsonl") {
            bail!(
                "scheduler fires root contains unsupported file {}",
                jsonl_path.display()
            );
        }
        super::crontab::validate_schedule_id(schedule_id)?;
        rebuild_fire_projection(&jsonl_path, schedule_id, db)?;
    }

    db.finish_fire_projection_rebuild()?;
    Ok(())
}

/// Rebuild `schedule_fires` from an already-pinned runtime root.
pub fn rebuild_fires_from_runtime_directory(
    runtime_directory: &lillux::PinnedDirectory,
    db: &SchedulerDb,
) -> Result<()> {
    db.begin_fire_projection_rebuild()?;
    let Some(schedules_directory) =
        runtime_directory.open_child_directory(std::ffi::OsStr::new("schedules"))?
    else {
        db.finish_fire_projection_rebuild()?;
        return Ok(());
    };

    for schedule_name in schedules_directory.entry_names()? {
        let schedule_id = schedule_name
            .to_str()
            .context("scheduler fire directory name must be UTF-8")?;
        super::crontab::validate_schedule_id(schedule_id)?;
        let schedule_directory = schedules_directory
            .open_child_directory(&schedule_name)?
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "scheduler fires root contains unsupported entry {}",
                    schedules_directory.path().join(&schedule_name).display()
                )
            })?;

        let mut has_journal = false;
        for entry_name in schedule_directory.entry_names()? {
            match entry_name.to_str() {
                Some(".fires.jsonl.lock") => {
                    schedule_directory
                        .open_regular(&entry_name, false)?
                        .ok_or_else(|| {
                            anyhow::anyhow!(
                                "scheduler fire lock is not a regular file: {}",
                                schedule_directory.path().join(&entry_name).display()
                            )
                        })?;
                }
                Some("fires.jsonl") => {
                    schedule_directory
                        .open_regular(&entry_name, false)?
                        .ok_or_else(|| {
                            anyhow::anyhow!(
                                "scheduler fire journal is not a regular file: {}",
                                schedule_directory.path().join(&entry_name).display()
                            )
                        })?;
                    has_journal = true;
                }
                _ => bail!(
                    "scheduler fire directory contains unsupported entry {}",
                    schedule_directory.path().join(&entry_name).display()
                ),
            }
        }
        if has_journal {
            let name = std::ffi::OsStr::new("fires.jsonl");
            let lock = lillux::ExclusiveFileLock::acquire_existing_in(&schedule_directory, name)?;
            rebuild_fire_projection_with_lock(
                &lock,
                &schedule_directory.path().join(name),
                schedule_id,
                db,
            )?;
        }
    }

    db.finish_fire_projection_rebuild()?;
    Ok(())
}

/// Parse a canonical JSONL journal and upsert its latest legal snapshots.
fn rebuild_fire_projection(
    jsonl_path: &Path,
    expected_schedule: &str,
    db: &SchedulerDb,
) -> Result<()> {
    let lock = lillux::ExclusiveFileLock::acquire(jsonl_path)?;
    rebuild_fire_projection_with_lock(&lock, jsonl_path, expected_schedule, db)
}

fn rebuild_fire_projection_with_lock(
    lock: &lillux::ExclusiveFileLock,
    jsonl_path: &Path,
    expected_schedule: &str,
    db: &SchedulerDb,
) -> Result<()> {
    let file = lock
        .open_target_read()
        .with_context(|| format!("read {}", jsonl_path.display()))?;
    let reader = std::io::BufReader::new(file);

    let mut latest: std::collections::HashMap<String, FireRecord> =
        std::collections::HashMap::new();

    for (line_number, line) in reader.lines().enumerate() {
        let line = line.with_context(|| format!("read {}", jsonl_path.display()))?;
        if line.is_empty() {
            bail!(
                "fire journal {} has an empty line at {}",
                jsonl_path.display(),
                line_number + 1
            );
        }
        let rec: FireRecord = serde_json::from_str(&line).with_context(|| {
            format!(
                "decode fire snapshot in {} at line {}",
                jsonl_path.display(),
                line_number + 1
            )
        })?;
        let canonical = rec.canonical_json_line()?;
        if canonical != line {
            bail!(
                "fire journal {} line {} is not canonical JSON",
                jsonl_path.display(),
                line_number + 1
            );
        }
        if rec.schedule_id != expected_schedule {
            bail!(
                "fire {} belongs to schedule {} but is stored under {}",
                rec.fire_id,
                rec.schedule_id,
                expected_schedule,
            );
        }
        if let Some(previous) = latest.get(&rec.fire_id) {
            rec.validate_transition_from(previous).with_context(|| {
                format!(
                    "illegal fire transition in {} at line {}",
                    jsonl_path.display(),
                    line_number + 1
                )
            })?;
        }
        latest.insert(rec.fire_id.clone(), rec);
    }

    for rec in latest.into_values() {
        db.project_fire_from_journal(&rec)
            .with_context(|| format!("project rebuilt scheduler fire {}", rec.fire_id))?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    const TEST_KEY: [u8; 32] = [42; 32];

    fn test_db() -> SchedulerDb {
        SchedulerDb::new_in_memory().expect("open in-memory scheduler db")
    }

    fn schedule_dir(root: &Path) -> PathBuf {
        root.join(ryeos_engine::AI_DIR)
            .join("node")
            .join("schedules")
    }

    fn fires_root(root: &Path) -> PathBuf {
        root.join(ryeos_engine::AI_DIR)
            .join("state")
            .join("schedules")
    }

    fn fire_journal(root: &Path, schedule_id: &str) -> PathBuf {
        fires_root(root).join(schedule_id).join("fires.jsonl")
    }

    fn test_source(schedule_id: &str) -> ScheduleSourceRecord {
        ScheduleSourceRecord {
            spec_version: 1,
            schedule_id: schedule_id.to_owned(),
            item_ref: "directive:test/hello".to_owned(),
            ref_bindings: std::collections::BTreeMap::new(),
            schedule_type: "interval".to_owned(),
            expression: "60".to_owned(),
            params: serde_json::json!({}),
            timezone: "UTC".to_owned(),
            misfire_policy: "fire_once_now".to_owned(),
            overlap_policy: "skip".to_owned(),
            lateness_grace_secs: 60,
            enabled: true,
            project_root: None,
            registered_at: 1_700_000_000_000,
            execution: super::super::types::ScheduleExecution {
                requester_fingerprint: "requester:test".to_owned(),
                capabilities: vec!["ryeos.execute.*".to_owned()],
            },
            managed_by: None,
        }
    }

    fn test_trust_store(key: [u8; 32]) -> ryeos_engine::trust::TrustStore {
        let signing_key = lillux::crypto::SigningKey::from_bytes(&key);
        let verifying_key = lillux::crypto::VerifyingKey::from(&signing_key);
        let fingerprint = lillux::cas::sha256_hex(verifying_key.to_bytes().as_ref());
        ryeos_engine::trust::TrustStore::from_signers(vec![ryeos_engine::trust::TrustedSigner {
            fingerprint,
            verifying_key,
            label: Some("scheduler-test-signer".to_owned()),
        }])
    }

    fn write_signed_body(
        schedules_dir: &Path,
        filename: &str,
        body: &str,
        key: [u8; 32],
    ) -> String {
        fs::create_dir_all(schedules_dir).unwrap();
        let signing_key = lillux::crypto::SigningKey::from_bytes(&key);
        let signed = lillux::signature::sign_content(body, &signing_key, "#", None);
        fs::write(schedules_dir.join(filename), &signed).unwrap();
        signed
    }

    fn write_signed_source(
        schedules_dir: &Path,
        filename: &str,
        source: &ScheduleSourceRecord,
        key: [u8; 32],
    ) -> String {
        let body = serde_yaml::to_string(source).unwrap();
        write_signed_body(schedules_dir, filename, &body, key)
    }

    fn test_fire(schedule_id: &str, scheduled_at: i64, status: &str) -> FireRecord {
        let fire_id = super::super::types::fire_id(schedule_id, scheduled_at);
        FireRecord {
            fire_id: fire_id.clone(),
            schedule_id: schedule_id.to_owned(),
            scheduled_at,
            fired_at: Some(scheduled_at),
            completed_at: (status != "dispatched").then_some(scheduled_at + 1),
            thread_id: (status != "skipped")
                .then(|| super::super::types::thread_id_from_fire(&fire_id)),
            status: status.to_owned(),
            trigger_reason: "normal".to_owned(),
            outcome: match status {
                "dispatched" => None,
                "completed" => Some("success".to_owned()),
                "cancelled" => Some("thread_cancelled".to_owned()),
                "skipped" => Some("normal".to_owned()),
                _ => Some("thread_failed".to_owned()),
            },
            signer_fingerprint: "11".repeat(32),
        }
    }

    #[test]
    fn schedule_source_requires_explicit_nullable_keys() {
        for key in ["project_root", "managed_by"] {
            let mut value = serde_json::to_value(test_source("strict-wire")).unwrap();
            value.as_object_mut().unwrap().remove(key);
            let error = serde_json::from_value::<ScheduleSourceRecord>(value).unwrap_err();
            assert!(error.to_string().contains(key), "got: {error}");
            assert!(error.to_string().contains("missing field"), "got: {error}");
        }

        let value = serde_json::to_value(test_source("strict-wire")).unwrap();
        assert!(value.get("project_root").unwrap().is_null());
        assert!(value.get("managed_by").unwrap().is_null());
    }

    #[test]
    fn schedule_source_rejects_unknown_structural_fields() {
        let mut value = serde_json::to_value(test_source("strict-wire")).unwrap();
        value
            .as_object_mut()
            .unwrap()
            .insert("section".to_owned(), serde_json::json!("schedules"));
        let error = serde_json::from_value::<ScheduleSourceRecord>(value).unwrap_err();
        assert!(error.to_string().contains("unknown field"));
        assert!(error.to_string().contains("section"));
    }

    #[test]
    fn trusted_canonical_source_rebuilds_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let schedules_dir = schedule_dir(dir.path());
        let signed = write_signed_source(
            &schedules_dir,
            "trusted.yaml",
            &test_source("trusted"),
            TEST_KEY,
        );
        let envelope = ryeos_engine::contracts::SignatureEnvelope {
            prefix: "#".to_owned(),
            suffix: None,
            after_shebang: false,
        };
        let header =
            ryeos_engine::item_resolution::parse_signature_header(&signed, &envelope).unwrap();

        let db = test_db();
        let live_ids =
            rebuild_specs_from_dir(&schedules_dir, &db, &test_trust_store(TEST_KEY)).unwrap();

        assert_eq!(live_ids, vec!["trusted"]);
        let spec = db.get_spec("trusted").unwrap().unwrap();
        assert_eq!(spec.spec_hash, header.content_hash);
        assert_eq!(spec.signer_fingerprint, header.signer_fingerprint);
        assert_eq!(spec.registered_at, 1_700_000_000_000);
    }

    #[test]
    fn source_failure_keeps_previous_complete_projection() {
        let dir = tempfile::tempdir().unwrap();
        let schedules_dir = schedule_dir(dir.path());
        write_signed_source(&schedules_dir, "new.yaml", &test_source("new"), TEST_KEY);
        fs::write(schedules_dir.join("unexpected.txt"), "not authoritative").unwrap();

        let db = test_db();
        let previous = test_source("previous")
            .to_spec_record(&"22".repeat(32), &"33".repeat(32))
            .unwrap();
        db.replace_specs(&[previous]).unwrap();

        let error =
            rebuild_specs_from_dir(&schedules_dir, &db, &test_trust_store(TEST_KEY)).unwrap_err();
        assert!(error.to_string().contains("unsupported non-.yaml file"));
        assert!(db.get_spec("previous").unwrap().is_some());
        assert!(db.get_spec("new").unwrap().is_none());
    }

    #[test]
    fn source_directory_rejects_every_unsupported_entry() {
        let dir = tempfile::tempdir().unwrap();
        let schedules_dir = schedule_dir(dir.path());
        fs::create_dir_all(&schedules_dir).unwrap();
        fs::write(schedules_dir.join("README"), "unexpected").unwrap();

        let error = rebuild_specs_from_dir(&schedules_dir, &test_db(), &test_trust_store(TEST_KEY))
            .unwrap_err();
        assert!(error.to_string().contains("unsupported non-.yaml file"));
    }

    #[test]
    fn source_filename_identity_must_match_signed_record() {
        let dir = tempfile::tempdir().unwrap();
        let schedules_dir = schedule_dir(dir.path());
        write_signed_source(
            &schedules_dir,
            "claimed.yaml",
            &test_source("actual"),
            TEST_KEY,
        );

        let error = rebuild_specs_from_dir(&schedules_dir, &test_db(), &test_trust_store(TEST_KEY))
            .unwrap_err();
        assert!(format!("{error:#}").contains("declares schedule_id"));
    }

    #[test]
    fn source_missing_required_authority_is_a_hard_error() {
        let dir = tempfile::tempdir().unwrap();
        let schedules_dir = schedule_dir(dir.path());
        let mut value = serde_json::to_value(test_source("no-execution")).unwrap();
        value.as_object_mut().unwrap().remove("execution");
        let body = serde_yaml::to_string(&value).unwrap();
        write_signed_body(&schedules_dir, "no-execution.yaml", &body, TEST_KEY);

        let error = rebuild_specs_from_dir(&schedules_dir, &test_db(), &test_trust_store(TEST_KEY))
            .unwrap_err();
        assert!(format!("{error:#}").contains("missing field"));
    }

    #[test]
    fn unsigned_untrusted_and_tampered_sources_fail_closed() {
        let unsigned_dir = tempfile::tempdir().unwrap();
        let unsigned_schedules = schedule_dir(unsigned_dir.path());
        fs::create_dir_all(&unsigned_schedules).unwrap();
        fs::write(
            unsigned_schedules.join("unsigned.yaml"),
            serde_yaml::to_string(&test_source("unsigned")).unwrap(),
        )
        .unwrap();
        assert!(rebuild_specs_from_dir(
            &unsigned_schedules,
            &test_db(),
            &test_trust_store(TEST_KEY),
        )
        .is_err());

        let untrusted_dir = tempfile::tempdir().unwrap();
        let untrusted_schedules = schedule_dir(untrusted_dir.path());
        write_signed_source(
            &untrusted_schedules,
            "untrusted.yaml",
            &test_source("untrusted"),
            TEST_KEY,
        );
        assert!(rebuild_specs_from_dir(
            &untrusted_schedules,
            &test_db(),
            &test_trust_store([99; 32]),
        )
        .is_err());

        let tampered_dir = tempfile::tempdir().unwrap();
        let tampered_schedules = schedule_dir(tampered_dir.path());
        let signed = write_signed_source(
            &tampered_schedules,
            "tampered.yaml",
            &test_source("tampered"),
            TEST_KEY,
        );
        let tampered = signed.replace("enabled: true", "enabled: false");
        assert_ne!(tampered, signed);
        fs::write(tampered_schedules.join("tampered.yaml"), tampered).unwrap();
        assert!(rebuild_specs_from_dir(
            &tampered_schedules,
            &test_db(),
            &test_trust_store(TEST_KEY),
        )
        .is_err());
    }

    #[test]
    fn absent_source_directory_replaces_projection_with_empty_set() {
        let dir = tempfile::tempdir().unwrap();
        let db = test_db();
        let previous = test_source("previous")
            .to_spec_record(&"22".repeat(32), &"33".repeat(32))
            .unwrap();
        db.replace_specs(&[previous]).unwrap();

        let live_ids =
            rebuild_specs_from_dir(&schedule_dir(dir.path()), &db, &test_trust_store(TEST_KEY))
                .unwrap();

        assert!(live_ids.is_empty());
        assert!(db.get_spec("previous").unwrap().is_none());
    }

    #[test]
    fn append_writes_only_canonical_fire_snapshots() {
        let dir = tempfile::tempdir().unwrap();
        let path = fire_journal(dir.path(), "test");
        let dispatched = test_fire("test", 1_000, "dispatched");
        let completed = test_fire("test", 1_000, "completed");

        append_jsonl_entry(&path, &dispatched).unwrap();
        append_jsonl_entry(&path, &completed).unwrap();

        let contents = fs::read_to_string(&path).unwrap();
        let lines = contents.lines().collect::<Vec<_>>();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0], dispatched.canonical_json_line().unwrap());
        assert_eq!(lines[1], completed.canonical_json_line().unwrap());
    }

    #[test]
    fn append_repairs_only_a_crash_torn_tail() {
        let dir = tempfile::tempdir().unwrap();
        let path = fire_journal(dir.path(), "test");
        let dispatched = test_fire("test", 1_000, "dispatched");
        let completed = test_fire("test", 1_000, "completed");
        append_jsonl_entry(&path, &dispatched).unwrap();

        let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(br#"{"fire_id":"torn"#).unwrap();
        file.sync_all().unwrap();
        drop(file);

        append_jsonl_entry(&path, &completed).unwrap();

        let contents = fs::read_to_string(&path).unwrap();
        let lines = contents.lines().collect::<Vec<_>>();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0], dispatched.canonical_json_line().unwrap());
        assert_eq!(lines[1], completed.canonical_json_line().unwrap());
    }

    #[test]
    fn append_rejects_noncanonical_path_and_partial_record() {
        let dir = tempfile::tempdir().unwrap();
        let wrong_path = dir.path().join("fires.jsonl");
        assert!(append_jsonl_entry(&wrong_path, &test_fire("test", 1_000, "dispatched"),).is_err());

        let path = fire_journal(dir.path(), "test");
        assert!(append_jsonl_entry(&path, &serde_json::json!({"fire_id": "test@1000"})).is_err());
    }

    #[test]
    fn replay_projects_latest_legal_transition() {
        let dir = tempfile::tempdir().unwrap();
        let path = fire_journal(dir.path(), "test");
        append_jsonl_entry(&path, &test_fire("test", 1_000, "dispatched")).unwrap();
        append_jsonl_entry(&path, &test_fire("test", 1_000, "completed")).unwrap();
        append_jsonl_entry(&path, &test_fire("test", 2_000, "dispatched")).unwrap();

        let db = test_db();
        rebuild_fires_from_dir(&fires_root(dir.path()), &db).unwrap();

        assert!(db.fire_projection_is_current().unwrap());
        assert_eq!(
            db.get_fire("test@1000").unwrap().unwrap().status,
            "completed"
        );
        assert_eq!(
            db.get_fire("test@2000").unwrap().unwrap().status,
            "dispatched"
        );
    }

    #[test]
    fn replay_rejects_illegal_or_noncanonical_history() {
        let illegal_dir = tempfile::tempdir().unwrap();
        let illegal_path = fire_journal(illegal_dir.path(), "test");
        let dispatched = test_fire("test", 1_000, "dispatched");
        let mut completed = test_fire("test", 1_000, "completed");
        completed.fired_at = Some(1_001);
        completed.completed_at = Some(1_002);
        append_jsonl_entry(&illegal_path, &dispatched).unwrap();
        append_jsonl_entry(&illegal_path, &completed).unwrap();

        let illegal_db = test_db();
        assert!(rebuild_fires_from_dir(&fires_root(illegal_dir.path()), &illegal_db).is_err());
        assert!(!illegal_db.fire_projection_is_current().unwrap());

        let noncanonical_dir = tempfile::tempdir().unwrap();
        let noncanonical_path = fire_journal(noncanonical_dir.path(), "test");
        fs::create_dir_all(noncanonical_path.parent().unwrap()).unwrap();
        let line = test_fire("test", 1_000, "dispatched")
            .canonical_json_line()
            .unwrap();
        fs::write(&noncanonical_path, format!(" {line}\n")).unwrap();

        let noncanonical_db = test_db();
        assert!(
            rebuild_fires_from_dir(&fires_root(noncanonical_dir.path()), &noncanonical_db,)
                .is_err()
        );
        assert!(!noncanonical_db.fire_projection_is_current().unwrap());
    }

    #[test]
    fn replay_rejects_cross_schedule_journal_identity() {
        let dir = tempfile::tempdir().unwrap();
        let path = fire_journal(dir.path(), "claimed");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let line = test_fire("actual", 1_000, "dispatched")
            .canonical_json_line()
            .unwrap();
        fs::write(path, format!("{line}\n")).unwrap();

        assert!(rebuild_fires_from_dir(&fires_root(dir.path()), &test_db()).is_err());
    }

    #[test]
    fn absent_fire_root_publishes_an_empty_current_projection() {
        let dir = tempfile::tempdir().unwrap();
        let db = test_db();

        rebuild_fires_from_dir(&fires_root(dir.path()), &db).unwrap();

        assert!(db.fire_projection_is_current().unwrap());
    }
}
