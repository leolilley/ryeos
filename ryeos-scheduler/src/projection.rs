//! Projection: sync between CAS files and `scheduler.sqlite3`.
//!
//! Schedule specs live as signed YAML in `.ai/node/schedules/`.
//! Fire history lives as JSONL in `.ai/state/schedules/*/fires.jsonl`.
//! The projection DB indexes both. Nuke the DB → rebuild from these files.

use std::fs;
use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result};

use super::db::SchedulerDb;
use super::types::{FireRecord, ScheduleSpecRecord};

/// Append a JSON value as a single line to a JSONL file.
/// Creates parent directories and the file if they don't exist.
pub fn append_jsonl_entry(path: &Path, entry: &serde_json::Value) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create fires dir {}", parent.display()))?;
    }
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("open fires.jsonl at {}", path.display()))?;
    let mut line = serde_json::to_string(entry)
        .with_context(|| "serialize fire entry")?;
    line.push('\n');
    file.write_all(line.as_bytes())
        .with_context(|| "write fire entry")?;
    Ok(())
}

/// Rebuild `schedule_specs` from `.ai/node/schedules/*.yaml`.
/// After calling this, call `delete_stale_specs` to remove projections
/// for YAML files that no longer exist.
///
/// When a [`ryeos_engine::trust::TrustStore`] is provided, full Ed25519
/// signature verification is performed: the signer fingerprint is looked
/// up in the trust store and the cryptographic signature is verified
/// against the content hash. Schedules signed by untrusted keys are
/// rejected. When `None`, only content_hash integrity is checked.
pub fn rebuild_specs_from_dir(
    schedules_dir: &Path,
    db: &SchedulerDb,
    trust_store: Option<&ryeos_engine::trust::TrustStore>,
) -> Result<Vec<String>> {
    let mut live_ids: Vec<String> = Vec::new();

    if !schedules_dir.is_dir() {
        return Ok(live_ids);
    }

    for entry in fs::read_dir(schedules_dir)
        .with_context(|| format!("read schedules dir {}", schedules_dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();

        if !path.is_file() || path.is_symlink() {
            continue;
        }

        let ext = path.extension().and_then(|e| e.to_str());
        if ext != Some("yaml") && ext != Some("yml") {
            continue;
        }

        let name = match path.file_stem().and_then(|s| s.to_str()) {
            Some(n) => n.to_string(),
            None => continue,
        };

        let content = match fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "failed to read schedule YAML — skipping");
                continue;
            }
        };

        // Strip signature lines and parse YAML body
        let body_str = lillux::signature::strip_signature_lines(&content);

        // Verify signature integrity: the signature's content_hash must match
        // the actual body hash. This catches tampering after signing.
        let signer_fingerprint = match parse_signer_fingerprint(&content) {
            Some(fp) => fp,
            None => {
                tracing::warn!(path = %path.display(), "missing or invalid signature — skipping unsigned schedule");
                continue;
            }
        };
        let expected_hash = lillux::cas::sha256_hex(body_str.as_bytes());
        let sig_header = content.lines().next()
            .and_then(|line| lillux::signature::parse_signature_line(line, "#", None));
        if let Some(ref header) = sig_header {
            if header.content_hash != expected_hash {
                tracing::warn!(
                    path = %path.display(),
                    expected = %expected_hash,
                    got = %header.content_hash,
                    "signature content_hash mismatch — skipping tampered schedule"
                );
                continue;
            }

            // Full Ed25519 signature verification when trust store is available.
            // Rejects schedules signed by untrusted keys — prevents forgery
            // even if someone creates a valid-looking YAML with a fake sig line.
            if let Some(ts) = trust_store {
                match ts.get(&header.signer_fingerprint) {
                    Some(trusted_signer) => {
                        if !lillux::signature::verify_signature(
                            &header.content_hash,
                            &header.signature_b64,
                            &trusted_signer.verifying_key,
                        ) {
                            tracing::warn!(
                                path = %path.display(),
                                signer = %header.signer_fingerprint,
                                "Ed25519 signature verification failed — skipping forged schedule"
                            );
                            continue;
                        }
                    }
                    None => {
                        tracing::warn!(
                            path = %path.display(),
                            signer = %header.signer_fingerprint,
                            "signer not in trust store — skipping untrusted schedule"
                        );
                        continue;
                    }
                }
            }
        } else {
            tracing::warn!(path = %path.display(), "could not parse signature header — skipping");
            continue;
        }

        let body: serde_json::Value = match serde_yaml::from_str(&body_str) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "failed to parse schedule YAML — skipping");
                continue;
            }
        };

        let schedule_id = match body.get("schedule_id").and_then(|v| v.as_str()) {
            Some(id) => id.to_string(),
            None => {
                tracing::warn!(path = %path.display(), "missing schedule_id — skipping");
                continue;
            }
        };

        // Validate schedule_id format (no path traversal, no whitespace)
        if let Err(e) = super::crontab::validate_schedule_id(&schedule_id) {
            tracing::warn!(path = %path.display(), schedule_id = %schedule_id, error = %e, "invalid schedule_id — skipping");
            continue;
        }

        if schedule_id != name {
            tracing::warn!(
                path = %path.display(),
                expected = %name,
                got = %schedule_id,
                "schedule_id != filename stem — skipping"
            );
            continue;
        }

        let spec_hash = lillux::cas::sha256_hex(content.as_bytes());
        // registered_at is a required field — the immutable scheduling anchor.
        // Set once at registration, never omitted. If missing, the YAML is
        // invalid — reject it.
        let registered_at = match body.get("registered_at").and_then(|v| v.as_i64()) {
            Some(ts) => ts,
            None => {
                tracing::warn!(
                    path = %path.display(),
                    schedule_id = %name,
                    "schedule missing required field 'registered_at' — rejecting invalid schedule"
                );
                continue;
            }
        };

        let rec = match spec_record_from_body(&body, &signer_fingerprint, &spec_hash, registered_at) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    schedule_id = %name,
                    error = %e,
                    "schedule validation failed — rejecting"
                );
                continue;
            }
        };

        if let Err(e) = db.upsert_spec(&rec) {
            tracing::error!(schedule_id = %schedule_id, error = %e, "failed to upsert spec projection");
        }

        live_ids.push(schedule_id);
    }

    Ok(live_ids)
}

/// Rebuild `schedule_fires` from `.ai/state/schedules/*/fires.jsonl`.
pub fn rebuild_fires_from_dir(
    fires_dir: &Path,
    db: &SchedulerDb,
) -> Result<()> {
    if !fires_dir.is_dir() {
        return Ok(());
    }

    for entry in fs::read_dir(fires_dir)
        .with_context(|| format!("read fires dir {}", fires_dir.display()))?
    {
        let entry = entry?;
        let schedule_dir = entry.path();
        if !schedule_dir.is_dir() {
            continue;
        }

        // Validate directory name as a schedule_id to prevent path traversal
        let dir_name = match schedule_dir.file_name().and_then(|n| n.to_str()) {
            Some(n) => n,
            None => continue,
        };
        if let Err(e) = super::crontab::validate_schedule_id(dir_name) {
            tracing::warn!(
                path = %schedule_dir.display(),
                error = %e,
                "skipping fires dir with invalid schedule_id name"
            );
            continue;
        }

        let jsonl_path = schedule_dir.join("fires.jsonl");
        if !jsonl_path.is_file() {
            continue;
        }

        rebuild_fire_projection(&jsonl_path, db)?;
    }

    Ok(())
}

/// Parse a JSONL file and upsert fire records.
/// Last entry for a given `fire_id` wins (self-contained snapshots).
fn rebuild_fire_projection(jsonl_path: &Path, db: &SchedulerDb) -> Result<()> {
    let content = fs::read_to_string(jsonl_path)
        .with_context(|| format!("read {}", jsonl_path.display()))?;

    // Collect entries by fire_id (last wins)
    let mut latest: std::collections::HashMap<String, FireRecord> =
        std::collections::HashMap::new();

    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let entry: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(path = %jsonl_path.display(), error = %e, "skipping malformed JSONL line");
                continue;
            }
        };

        let fire_id = match entry.get("fire_id").and_then(|v| v.as_str()) {
            Some(id) => id.to_string(),
            None => continue,
        };

        let rec = fire_record_from_entry(&entry);
        latest.insert(fire_id, rec);
    }

    for rec in latest.into_values() {
        if let Err(e) = db.upsert_fire(&rec) {
            tracing::error!(fire_id = %rec.fire_id, error = %e, "failed to upsert fire projection");
        }
    }

    Ok(())
}

fn fire_record_from_entry(entry: &serde_json::Value) -> FireRecord {
    FireRecord {
        fire_id: entry_str(entry, "fire_id"),
        schedule_id: entry_str(entry, "schedule_id"),
        scheduled_at: entry_int(entry, "scheduled_at"),
        fired_at: entry.get("fired_at").and_then(|v| v.as_i64()),
        completed_at: entry.get("completed_at").and_then(|v| v.as_i64()),
        thread_id: entry.get("thread_id").and_then(|v| v.as_str()).map(String::from),
        status: entry_str(entry, "status"),
        trigger_reason: entry.get("trigger_reason").and_then(|v| v.as_str()).unwrap_or("normal").to_string(),
        outcome: entry.get("outcome").and_then(|v| v.as_str()).map(String::from),
        signer_fingerprint: entry.get("signer_fingerprint").and_then(|v| v.as_str()).map(String::from),
    }
}

fn entry_str(v: &serde_json::Value, key: &str) -> String {
    v.get(key).and_then(|v| v.as_str()).unwrap_or("").to_string()
}

fn entry_int(v: &serde_json::Value, key: &str) -> i64 {
    v.get(key).and_then(|v| v.as_i64()).unwrap_or(0)
}

fn spec_record_from_body(
    body: &serde_json::Value,
    signer_fingerprint: &str,
    spec_hash: &str,
    registered_at: i64,
) -> anyhow::Result<ScheduleSpecRecord> {
    // Fail-closed: execution block is required for security.
    // Reject schedules missing it — they must be re-registered.
    let (requester_fingerprint, capabilities) = body.get("execution")
        .and_then(|exec| {
            let fp = exec.get("requester_fingerprint")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())?;
            let caps = exec.get("capabilities")
                .and_then(|v| v.as_array())
                .filter(|arr| !arr.is_empty())?;
            let cap_strs: Vec<String> = caps.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect();
            if cap_strs.is_empty() { return None; }
            Some((fp.to_string(), cap_strs))
        })
        .ok_or_else(|| anyhow::anyhow!(
            "missing or invalid execution block (requires non-empty requester_fingerprint + capabilities)"
        ))?;

    // Normalize misfire default: both live and rebuild paths must use the
    // same default when the field is empty. Interval → fire_once_now, else → skip.
    let raw_misfire = body.get("misfire_policy")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let misfire_policy = if raw_misfire.is_empty() {
        match body.get("schedule_type").and_then(|v| v.as_str()).unwrap_or("") {
            "interval" => "fire_once_now".to_string(),
            _ => "skip".to_string(),
        }
    } else {
        raw_misfire.to_string()
    };

    Ok(ScheduleSpecRecord {
        schedule_id: body_str(body, "schedule_id"),
        item_ref: body_str(body, "item_ref"),
        params: body.get("params")
            .map(|v| serde_json::to_string(v).unwrap_or_default())
            .unwrap_or_default(),
        schedule_type: body_str(body, "schedule_type"),
        expression: body_str(body, "expression"),
        timezone: body.get("timezone").and_then(|v| v.as_str()).unwrap_or("UTC").to_string(),
        misfire_policy,
        overlap_policy: body.get("overlap_policy")
            .and_then(|v| v.as_str())
            .unwrap_or("skip")
            .to_string(),
        enabled: body.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true),
        project_root: body.get("project_root").and_then(|v| v.as_str()).map(String::from),
        signer_fingerprint: signer_fingerprint.to_string(),
        spec_hash: spec_hash.to_string(),
        registered_at,
        requester_fingerprint,
        capabilities,
    })
}

fn body_str(v: &serde_json::Value, key: &str) -> String {
    v.get(key).and_then(|v| v.as_str()).unwrap_or("").to_string()
}

pub fn parse_signer_fingerprint_from_str(content: &str) -> Option<String> {
    parse_signer_fingerprint(content)
}

fn parse_signer_fingerprint(content: &str) -> Option<String> {
    let first_line = content.lines().next()?;
    let header = lillux::signature::parse_signature_line(first_line, "#", None)?;
    Some(header.signer_fingerprint)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn test_db() -> SchedulerDb {
        SchedulerDb::open(&PathBuf::from(":memory:")).expect("open in-memory scheduler db")
    }

    // ── append_jsonl_entry ─────────────────────────────────────

    #[test]
    fn append_jsonl_creates_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fires.jsonl");

        let entry = serde_json::json!({"fire_id": "test@1000", "status": "dispatched"});
        append_jsonl_entry(&path, &entry).unwrap();

        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("test@1000"));
    }

    #[test]
    fn append_jsonl_appends_multiple() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fires.jsonl");

        append_jsonl_entry(&path, &serde_json::json!({"fire_id": "a"})).unwrap();
        append_jsonl_entry(&path, &serde_json::json!({"fire_id": "b"})).unwrap();

        let content = fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 2);
    }

    // ── rebuild_specs_from_dir ─────────────────────────────────

    #[test]
    fn rebuild_from_dir_reads_yaml() {
        let dir = tempfile::tempdir().unwrap();
        let sched_dir = dir.path().join("schedules");
        fs::create_dir_all(&sched_dir).unwrap();

        let body = r#"spec_version: 1
section: schedules
schedule_id: my-schedule
item_ref: "directive:test/hello"
schedule_type: interval
expression: "60"
registered_at: 1700000000000
execution:
  requester_fingerprint: "fp:test"
  capabilities:
    - "ryeos.execute.*"
"#;
        // Sign with a real key so content_hash matches the body
        let sk = lillux::crypto::SigningKey::from_bytes(&[42u8; 32]);
        let yaml_content = lillux::signature::sign_content(body, &sk, "#", None);
        fs::write(sched_dir.join("my-schedule.yaml"), yaml_content).unwrap();

        let db = test_db();
        let live_ids = rebuild_specs_from_dir(&sched_dir, &db, None).unwrap();

        assert_eq!(live_ids, vec!["my-schedule"]);
        let spec = db.get_spec("my-schedule").unwrap().unwrap();
        assert_eq!(spec.schedule_type, "interval");
        assert_eq!(spec.expression, "60");
    }

    #[test]
    fn rebuild_from_dir_skips_non_yaml() {
        let dir = tempfile::tempdir().unwrap();
        let sched_dir = dir.path().join("schedules");
        fs::create_dir_all(&sched_dir).unwrap();
        fs::write(sched_dir.join("readme.txt"), "not a schedule").unwrap();

        let db = test_db();
        let live_ids = rebuild_specs_from_dir(&sched_dir, &db, None).unwrap();
        assert!(live_ids.is_empty());
    }

    #[test]
    fn rebuild_from_dir_skips_mismatched_id() {
        let dir = tempfile::tempdir().unwrap();
        let sched_dir = dir.path().join("schedules");
        fs::create_dir_all(&sched_dir).unwrap();

        let body = r#"spec_version: 1
section: schedules
schedule_id: wrong-id
item_ref: "directive:test/hello"
schedule_type: interval
expression: "60"
"#;
        let sk = lillux::crypto::SigningKey::from_bytes(&[42u8; 32]);
        let yaml = lillux::signature::sign_content(body, &sk, "#", None);
        fs::write(sched_dir.join("my-schedule.yaml"), yaml).unwrap();

        let db = test_db();
        let live_ids = rebuild_specs_from_dir(&sched_dir, &db, None).unwrap();
        assert!(live_ids.is_empty());
    }

    #[test]
    fn rebuild_from_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        let sched_dir = dir.path().join("schedules");
        fs::create_dir_all(&sched_dir).unwrap();

        let db = test_db();
        let live_ids = rebuild_specs_from_dir(&sched_dir, &db, None).unwrap();
        assert!(live_ids.is_empty());
    }

    #[test]
    fn rebuild_from_nonexistent_dir() {
        let db = test_db();
        let live_ids = rebuild_specs_from_dir(Path::new("/nonexistent/path"), &db, None).unwrap();
        assert!(live_ids.is_empty());
    }

    // ── rebuild_fires_from_dir ─────────────────────────────────

    #[test]
    fn rebuild_fires_from_jsonl() {
        let dir = tempfile::tempdir().unwrap();
        let sched_dir = dir.path().join("my-schedule");
        fs::create_dir_all(&sched_dir).unwrap();

        let jsonl = r#"{"fire_id":"s@1000","schedule_id":"s","scheduled_at":1000,"fired_at":1001,"thread_id":"t1","status":"completed","trigger_reason":"normal","outcome":"success"}
{"fire_id":"s@2000","schedule_id":"s","scheduled_at":2000,"fired_at":2001,"thread_id":"t2","status":"dispatched","trigger_reason":"normal"}
"#;
        fs::write(sched_dir.join("fires.jsonl"), jsonl).unwrap();

        let db = test_db();
        rebuild_fires_from_dir(dir.path(), &db).unwrap();

        let fire1 = db.get_fire("s@1000").unwrap().unwrap();
        assert_eq!(fire1.status, "completed");
        let fire2 = db.get_fire("s@2000").unwrap().unwrap();
        assert_eq!(fire2.status, "dispatched");
    }

    #[test]
    fn rebuild_fires_last_entry_wins() {
        let dir = tempfile::tempdir().unwrap();
        let sched_dir = dir.path().join("my-schedule");
        fs::create_dir_all(&sched_dir).unwrap();

        let jsonl = r#"{"fire_id":"s@1000","schedule_id":"s","scheduled_at":1000,"status":"dispatched"}
{"fire_id":"s@1000","schedule_id":"s","scheduled_at":1000,"status":"completed","outcome":"success"}
"#;
        fs::write(sched_dir.join("fires.jsonl"), jsonl).unwrap();

        let db = test_db();
        rebuild_fires_from_dir(dir.path(), &db).unwrap();

        let fire = db.get_fire("s@1000").unwrap().unwrap();
        assert_eq!(fire.status, "completed");
        assert_eq!(fire.outcome.unwrap(), "success");
    }

    #[test]
    fn rebuild_fires_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        let db = test_db();
        rebuild_fires_from_dir(dir.path(), &db).unwrap();
        // No panic = success
    }

    // ── strip_signature_lines ──────────────────────────────────

    #[test]
    fn strip_signature_removes_header() {
        let content = "# ryeos:signed:2026-01-01T00:00:00Z:abc123:sig==:fp:abc\nspec_version: 1\nschedule_id: test";
        let body = lillux::signature::strip_signature_lines(content);
        assert!(!body.contains("ryeos:signed:"));
        assert!(body.contains("spec_version: 1"));
    }

    #[test]
    fn strip_signature_no_header() {
        let content = "spec_version: 1\nschedule_id: test";
        let body = lillux::signature::strip_signature_lines(content);
        assert!(body.contains("spec_version: 1"));
    }

    // ── parse_signer_fingerprint ────────────────────────────────

    #[test]
    fn parse_fingerprint_valid() {
        let content = "# ryeos:signed:2026-01-01T00:00:00Z:abc123:c2ln:b64==:fp:test:abc123\nrest";
        let fp = parse_signer_fingerprint(content).unwrap();
        assert_eq!(fp, "abc123");
    }

    #[test]
    fn parse_fingerprint_no_signature() {
        let content = "no signature here";
        let fp = parse_signer_fingerprint(content);
        assert!(fp.is_none());
    }

    // ── parse_signer_fingerprint_from_str ───────────────────────

    #[test]
    fn public_wrapper_works() {
        let content = "# ryeos:signed:2026-01-01T00:00:00Z:abc:Sig==:fp:test:hello\nrest";
        assert_eq!(parse_signer_fingerprint_from_str(content), Some("hello".to_string()));
    }

    // ── Authority model acceptance tests (Phase 4) ──────────────

    /// Helper: sign a YAML body and write it to a temp schedules dir.
    fn sign_and_write(sched_dir: &Path, filename: &str, body: &str) {
        let sk = lillux::crypto::SigningKey::from_bytes(&[42u8; 32]);
        let yaml_content = lillux::signature::sign_content(body, &sk, "#", None);
        fs::write(sched_dir.join(filename), yaml_content).unwrap();
    }

    #[test]
    fn authority_rejects_missing_execution_block() {
        // A schedule YAML without an `execution` block must be rejected
        // during projection rebuild — fail-closed, no silent fallback.
        let dir = tempfile::tempdir().unwrap();
        let sched_dir = dir.path().join("schedules");
        fs::create_dir_all(&sched_dir).unwrap();

        let body = r#"spec_version: 1
section: schedules
schedule_id: no-exec
item_ref: "directive:test/hello"
schedule_type: interval
expression: "60"
registered_at: 1700000000000
"#;
        sign_and_write(&sched_dir, "no-exec.yaml", body);

        let db = test_db();
        let live_ids = rebuild_specs_from_dir(&sched_dir, &db, None).unwrap();

        // Must be rejected — no execution block
        assert!(live_ids.is_empty(), "schedule without execution block should be rejected");
        assert!(db.get_spec("no-exec").unwrap().is_none());
    }

    #[test]
    fn authority_rejects_empty_capabilities() {
        // A schedule with an execution block but empty capabilities list
        // must be rejected — would create a schedule that can never dispatch.
        let dir = tempfile::tempdir().unwrap();
        let sched_dir = dir.path().join("schedules");
        fs::create_dir_all(&sched_dir).unwrap();

        let body = r#"spec_version: 1
section: schedules
schedule_id: empty-caps
item_ref: "directive:test/hello"
schedule_type: interval
expression: "60"
registered_at: 1700000000000
execution:
  requester_fingerprint: "fp:test"
  capabilities: []
"#;
        sign_and_write(&sched_dir, "empty-caps.yaml", body);

        let db = test_db();
        let live_ids = rebuild_specs_from_dir(&sched_dir, &db, None).unwrap();

        assert!(live_ids.is_empty(), "schedule with empty capabilities should be rejected");
        assert!(db.get_spec("empty-caps").unwrap().is_none());
    }

    #[test]
    fn authority_rejects_empty_requester_fingerprint() {
        // A schedule with empty requester_fingerprint must be rejected —
        // dispatch requires a valid principal identity.
        let dir = tempfile::tempdir().unwrap();
        let sched_dir = dir.path().join("schedules");
        fs::create_dir_all(&sched_dir).unwrap();

        let body = r#"spec_version: 1
section: schedules
schedule_id: empty-fp
item_ref: "directive:test/hello"
schedule_type: interval
expression: "60"
registered_at: 1700000000000
execution:
  requester_fingerprint: ""
  capabilities:
    - "ryeos.execute.*"
"#;
        sign_and_write(&sched_dir, "empty-fp.yaml", body);

        let db = test_db();
        let live_ids = rebuild_specs_from_dir(&sched_dir, &db, None).unwrap();

        assert!(live_ids.is_empty(), "schedule with empty requester_fingerprint should be rejected");
        assert!(db.get_spec("empty-fp").unwrap().is_none());
    }

    #[test]
    fn authority_accepts_valid_execution_block() {
        // Happy path: valid execution block with both fields present and non-empty.
        // Verifies that the projected spec records the authority fields correctly.
        let dir = tempfile::tempdir().unwrap();
        let sched_dir = dir.path().join("schedules");
        fs::create_dir_all(&sched_dir).unwrap();

        let body = r#"spec_version: 1
section: schedules
schedule_id: valid-auth
item_ref: "directive:test/hello"
schedule_type: interval
expression: "60"
registered_at: 1700000000000
execution:
  requester_fingerprint: "fp:principal-abc"
  capabilities:
    - "ryeos.execute.tool.*"
    - "ryeos.execute.directive.*"
"#;
        sign_and_write(&sched_dir, "valid-auth.yaml", body);

        let db = test_db();
        let live_ids = rebuild_specs_from_dir(&sched_dir, &db, None).unwrap();

        assert_eq!(live_ids, vec!["valid-auth"]);
        let spec = db.get_spec("valid-auth").unwrap().unwrap();
        assert_eq!(spec.requester_fingerprint, "fp:principal-abc");
        assert_eq!(spec.capabilities, vec!["ryeos.execute.tool.*", "ryeos.execute.directive.*"]);
    }

    #[test]
    fn authority_tampered_content_hash_rejected() {
        // If someone modifies the YAML body after signing, the content_hash
        // in the signature line won't match the actual body hash.
        // Projection must reject this — prevents tampering.
        let dir = tempfile::tempdir().unwrap();
        let sched_dir = dir.path().join("schedules");
        fs::create_dir_all(&sched_dir).unwrap();

        let body = r#"spec_version: 1
section: schedules
schedule_id: tamper-test
item_ref: "directive:test/hello"
schedule_type: interval
expression: "60"
registered_at: 1700000000000
execution:
  requester_fingerprint: "fp:test"
  capabilities:
    - "ryeos.execute.*"
"#;
        let sk = lillux::crypto::SigningKey::from_bytes(&[42u8; 32]);
        let mut signed = lillux::signature::sign_content(body, &sk, "#", None);

        // Tamper with the body (after the signature line)
        signed = signed.replace("expression: \"60\"", "expression: \"1\"");

        fs::write(sched_dir.join("tamper-test.yaml"), signed).unwrap();

        let db = test_db();
        let live_ids = rebuild_specs_from_dir(&sched_dir, &db, None).unwrap();

        assert!(live_ids.is_empty(), "tampered schedule should be rejected");
        assert!(db.get_spec("tamper-test").unwrap().is_none());
    }

    #[test]
    fn authority_registered_at_preserved_from_yaml() {
        // The registered_at timestamp in the YAML body must be used exactly
        // as-is — the scheduling anchor is never synthesized or overridden.
        let dir = tempfile::tempdir().unwrap();
        let sched_dir = dir.path().join("schedules");
        fs::create_dir_all(&sched_dir).unwrap();

        let fixed_ts: i64 = 1700000000000; // a known timestamp
        let body = format!(r#"spec_version: 1
section: schedules
schedule_id: anchor-test
item_ref: "directive:test/hello"
schedule_type: interval
expression: "60"
registered_at: {fixed_ts}
execution:
  requester_fingerprint: "fp:test"
  capabilities:
    - "ryeos.execute.*"
"#);
        sign_and_write(&sched_dir, "anchor-test.yaml", &body);

        let db = test_db();
        let live_ids = rebuild_specs_from_dir(&sched_dir, &db, None).unwrap();

        assert_eq!(live_ids, vec!["anchor-test"]);
        let spec = db.get_spec("anchor-test").unwrap().unwrap();
        assert_eq!(spec.registered_at, fixed_ts, "registered_at should come from YAML body exactly");
    }

    #[test]
    fn authority_missing_registered_at_rejected() {
        // registered_at is a required field — no fallback to file mtime.
        let dir = tempfile::tempdir().unwrap();
        let sched_dir = dir.path().join("schedules");
        fs::create_dir_all(&sched_dir).unwrap();

        let body = r#"spec_version: 1
section: schedules
schedule_id: no-anchor
item_ref: "directive:test/hello"
schedule_type: interval
expression: "60"
execution:
  requester_fingerprint: "fp:test"
  capabilities:
    - "ryeos.execute.*"
"#;
        sign_and_write(&sched_dir, "no-anchor.yaml", body);

        let db = test_db();
        let live_ids = rebuild_specs_from_dir(&sched_dir, &db, None).unwrap();

        assert!(live_ids.is_empty(), "schedule missing registered_at should be rejected");
        assert!(db.get_spec("no-anchor").unwrap().is_none());
    }

    #[test]
    fn authority_unsigned_schedule_rejected() {
        // A schedule YAML without any signature line must be rejected.
        // All schedules must be signed by the node.
        let dir = tempfile::tempdir().unwrap();
        let sched_dir = dir.path().join("schedules");
        fs::create_dir_all(&sched_dir).unwrap();

        let body = r#"spec_version: 1
section: schedules
schedule_id: unsigned
item_ref: "directive:test/hello"
schedule_type: interval
expression: "60"
execution:
  requester_fingerprint: "fp:test"
  capabilities:
    - "ryeos.execute.*"
"#;
        // Write WITHOUT signing
        fs::write(sched_dir.join("unsigned.yaml"), body).unwrap();

        let db = test_db();
        let live_ids = rebuild_specs_from_dir(&sched_dir, &db, None).unwrap();

        assert!(live_ids.is_empty(), "unsigned schedule should be rejected");
        assert!(db.get_spec("unsigned").unwrap().is_none());
    }

    #[test]
    fn authority_misfire_default_interval_normalizes_to_fire_once_now() {
        // Interval schedules without an explicit misfire_policy should
        // normalize to "fire_once_now" at projection time.
        let dir = tempfile::tempdir().unwrap();
        let sched_dir = dir.path().join("schedules");
        fs::create_dir_all(&sched_dir).unwrap();

        let body = r#"spec_version: 1
section: schedules
schedule_id: misfire-default
item_ref: "directive:test/hello"
schedule_type: interval
expression: "60"
registered_at: 1700000000000
execution:
  requester_fingerprint: "fp:test"
  capabilities:
    - "ryeos.execute.*"
"#;
        sign_and_write(&sched_dir, "misfire-default.yaml", body);

        let db = test_db();
        rebuild_specs_from_dir(&sched_dir, &db, None).unwrap();

        let spec = db.get_spec("misfire-default").unwrap().unwrap();
        assert_eq!(spec.misfire_policy, "fire_once_now",
            "interval schedule without explicit misfire_policy should default to fire_once_now");
    }

    #[test]
    fn authority_misfire_default_cron_normalizes_to_skip() {
        // Cron schedules without an explicit misfire_policy should
        // normalize to "skip" at projection time.
        let dir = tempfile::tempdir().unwrap();
        let sched_dir = dir.path().join("schedules");
        fs::create_dir_all(&sched_dir).unwrap();

        let body = r#"spec_version: 1
section: schedules
schedule_id: cron-misfire
item_ref: "directive:test/hello"
schedule_type: cron
expression: "0 0 * * * *"
registered_at: 1700000000000
execution:
  requester_fingerprint: "fp:test"
  capabilities:
    - "ryeos.execute.*"
"#;
        sign_and_write(&sched_dir, "cron-misfire.yaml", body);

        let db = test_db();
        rebuild_specs_from_dir(&sched_dir, &db, None).unwrap();

        let spec = db.get_spec("cron-misfire").unwrap().unwrap();
        assert_eq!(spec.misfire_policy, "skip",
            "cron schedule without explicit misfire_policy should default to skip");
    }

    #[test]
    fn authority_missing_capabilities_key_rejected() {
        // Execution block with requester_fingerprint but no capabilities key at all.
        let dir = tempfile::tempdir().unwrap();
        let sched_dir = dir.path().join("schedules");
        fs::create_dir_all(&sched_dir).unwrap();

        let body = r#"spec_version: 1
section: schedules
schedule_id: no-caps-key
item_ref: "directive:test/hello"
schedule_type: interval
expression: "60"
registered_at: 1700000000000
execution:
  requester_fingerprint: "fp:test"
"#;
        sign_and_write(&sched_dir, "no-caps-key.yaml", body);

        let db = test_db();
        let live_ids = rebuild_specs_from_dir(&sched_dir, &db, None).unwrap();

        assert!(live_ids.is_empty(), "schedule missing capabilities key should be rejected");
    }

    // ── Full Ed25519 signature verification (Phase 6) ──────────

    fn test_trust_store() -> ryeos_engine::trust::TrustStore {
        // Build a trust store that trusts the test key ([42u8; 32])
        let sk = lillux::crypto::SigningKey::from_bytes(&[42u8; 32]);
        let vk = lillux::crypto::VerifyingKey::from(&sk);
        let fp = lillux::cas::sha256_hex(vk.to_bytes().as_ref());
        let signer = ryeos_engine::trust::TrustedSigner {
            fingerprint: fp,
            verifying_key: vk,
            label: Some("test-signer".to_string()),
        };
        ryeos_engine::trust::TrustStore::from_signers(vec![signer])
    }

    #[test]
    fn ed2559_trusted_signer_accepted() {
        // A schedule signed by a key in the trust store should pass
        // full Ed25519 verification.
        let dir = tempfile::tempdir().unwrap();
        let sched_dir = dir.path().join("schedules");
        fs::create_dir_all(&sched_dir).unwrap();

        let body = r#"spec_version: 1
section: schedules
schedule_id: trusted-sig
item_ref: "directive:test/hello"
schedule_type: interval
expression: "60"
registered_at: 1700000000000
execution:
  requester_fingerprint: "fp:test"
  capabilities:
    - "ryeos.execute.*"
"#;
        sign_and_write(&sched_dir, "trusted-sig.yaml", body);

        let db = test_db();
        let ts = test_trust_store();
        let live_ids = rebuild_specs_from_dir(&sched_dir, &db, Some(&ts)).unwrap();

        assert_eq!(live_ids, vec!["trusted-sig"]);
        let spec = db.get_spec("trusted-sig").unwrap().unwrap();
        assert_eq!(spec.schedule_id, "trusted-sig");
    }

    #[test]
    fn ed25519_untrusted_signer_rejected() {
        // A schedule signed by a key NOT in the trust store should be
        // rejected during full Ed25519 verification.
        let dir = tempfile::tempdir().unwrap();
        let sched_dir = dir.path().join("schedules");
        fs::create_dir_all(&sched_dir).unwrap();

        let body = r#"spec_version: 1
section: schedules
schedule_id: untrusted-sig
item_ref: "directive:test/hello"
schedule_type: interval
expression: "60"
registered_at: 1700000000000
execution:
  requester_fingerprint: "fp:test"
  capabilities:
    - "ryeos.execute.*"
"#;
        // Sign with the test key ([42u8; 32])
        sign_and_write(&sched_dir, "untrusted-sig.yaml", body);

        // Build a trust store with a DIFFERENT key
        let sk_other = lillux::crypto::SigningKey::from_bytes(&[99u8; 32]);
        let vk_other = lillux::crypto::VerifyingKey::from(&sk_other);
        let fp_other = lillux::cas::sha256_hex(vk_other.to_bytes().as_ref());
        let ts = ryeos_engine::trust::TrustStore::from_signers(vec![
            ryeos_engine::trust::TrustedSigner {
                fingerprint: fp_other,
                verifying_key: vk_other,
                label: Some("other-signer".to_string()),
            },
        ]);

        let db = test_db();
        let live_ids = rebuild_specs_from_dir(&sched_dir, &db, Some(&ts)).unwrap();

        assert!(live_ids.is_empty(), "schedule signed by untrusted key should be rejected");
        assert!(db.get_spec("untrusted-sig").unwrap().is_none());
    }

    #[test]
    fn ed25519_forged_signature_rejected() {
        // A schedule where the signature b64 is tampered with (but the
        // content_hash still matches) should be rejected by Ed25519
        // verification, even if the signer fingerprint is in the trust store.
        let dir = tempfile::tempdir().unwrap();
        let sched_dir = dir.path().join("schedules");
        fs::create_dir_all(&sched_dir).unwrap();

        let body = r#"spec_version: 1
section: schedules
schedule_id: forged-sig
item_ref: "directive:test/hello"
schedule_type: interval
expression: "60"
registered_at: 1700000000000
execution:
  requester_fingerprint: "fp:test"
  capabilities:
    - "ryeos.execute.*"
"#;
        let sk = lillux::crypto::SigningKey::from_bytes(&[42u8; 32]);
        let signed = lillux::signature::sign_content(body, &sk, "#", None);

        // The sig line format is: # ryeos:signed:<ts>:<hash>:<sig_b64>:<fp>
        // Parse using rsplitn(4, ':') to match the parser, then forge sig_b64.
        let line_end = signed.find('\n').expect("sig line ends with newline");
        let sig_line = &signed[..line_end];
        let rest = &signed[line_end + 1..];

        // Strip the "# " prefix, then "ryeos:signed:" prefix
        let after_marker = sig_line
            .strip_prefix("# ")
            .and_then(|s| s.strip_prefix(lillux::signature::SIGNATURE_PREFIX))
            .expect("valid sig line format");

        // rsplitn(4, ':') => [fp, sig_b64, hash, timestamp]
        let parts: Vec<&str> = after_marker.rsplitn(4, ':').collect();
        assert_eq!(parts.len(), 4, "sig line should have 4 rsplit parts");

        let forged_line = format!(
            "# {}{}:{}:{}:{}",
            lillux::signature::SIGNATURE_PREFIX,
            parts[3],  // timestamp
            parts[2],  // content_hash (untouched)
            "FORGEDSIGNATUREBASE64==", // bogus sig_b64
            parts[0],  // fingerprint (untouched)
        );
        let forged_content = format!("{}\n{}", forged_line, rest);

        fs::write(sched_dir.join("forged-sig.yaml"), forged_content).unwrap();

        let db = test_db();
        let ts = test_trust_store();
        let live_ids = rebuild_specs_from_dir(&sched_dir, &db, Some(&ts)).unwrap();

        assert!(live_ids.is_empty(), "forged signature should be rejected by Ed25519 verification");
        assert!(db.get_spec("forged-sig").unwrap().is_none());
    }

    #[test]
    fn ed25519_none_trust_store_falls_back_to_hash_only() {
        // Without a trust store (None), schedules are accepted if the
        // content_hash matches — the existing behavior. This ensures
        // backward compatibility for code paths that don't have a
        // trust store available.
        let dir = tempfile::tempdir().unwrap();
        let sched_dir = dir.path().join("schedules");
        fs::create_dir_all(&sched_dir).unwrap();

        let body = r#"spec_version: 1
section: schedules
schedule_id: hash-only
item_ref: "directive:test/hello"
schedule_type: interval
expression: "60"
registered_at: 1700000000000
execution:
  requester_fingerprint: "fp:test"
  capabilities:
    - "ryeos.execute.*"
"#;
        sign_and_write(&sched_dir, "hash-only.yaml", body);

        let db = test_db();
        let live_ids = rebuild_specs_from_dir(&sched_dir, &db, None).unwrap();

        assert_eq!(live_ids, vec!["hash-only"]);
    }
}
