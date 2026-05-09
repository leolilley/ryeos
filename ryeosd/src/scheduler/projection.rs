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
pub fn rebuild_specs_from_dir(
    schedules_dir: &Path,
    db: &SchedulerDb,
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
        let body_str = strip_signature(&content);
        let body: serde_json::Value = match serde_yaml::from_str(&body_str) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "failed to parse schedule YAML — skipping");
                continue;
            }
        };

        // Extract signer fingerprint from signature line
        let signer_fingerprint = parse_signer_fingerprint(&content)
            .unwrap_or_else(|| "unknown".to_string());

        let schedule_id = match body.get("schedule_id").and_then(|v| v.as_str()) {
            Some(id) => id.to_string(),
            None => {
                tracing::warn!(path = %path.display(), "missing schedule_id — skipping");
                continue;
            }
        };

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
        let mtime = fs::metadata(&path)
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as i64)
            .unwrap_or_else(lillux::time::timestamp_millis);

        let rec = spec_record_from_body(&body, &signer_fingerprint, &spec_hash, mtime);

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
    last_modified: i64,
) -> ScheduleSpecRecord {
    ScheduleSpecRecord {
        schedule_id: body_str(body, "schedule_id"),
        item_ref: body_str(body, "item_ref"),
        params: body.get("params")
            .map(|v| serde_json::to_string(v).unwrap_or_default())
            .unwrap_or_default(),
        schedule_type: body_str(body, "schedule_type"),
        expression: body_str(body, "expression"),
        timezone: body.get("timezone").and_then(|v| v.as_str()).unwrap_or("UTC").to_string(),
        misfire_policy: body.get("misfire_policy")
            .and_then(|v| v.as_str())
            .unwrap_or("skip")
            .to_string(),
        overlap_policy: body.get("overlap_policy")
            .and_then(|v| v.as_str())
            .unwrap_or("skip")
            .to_string(),
        enabled: body.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true),
        project_root: body.get("project_root").and_then(|v| v.as_str()).map(String::from),
        signer_fingerprint: signer_fingerprint.to_string(),
        spec_hash: spec_hash.to_string(),
        last_modified,
    }
}

fn body_str(v: &serde_json::Value, key: &str) -> String {
    v.get(key).and_then(|v| v.as_str()).unwrap_or("").to_string()
}

fn strip_signature(content: &str) -> String {
    content
        .lines()
        .skip_while(|l| l.trim().starts_with("# ryeos:signed:"))
        .collect::<Vec<_>>()
        .join("\n")
        .trim_start()
        .to_string()
}

pub fn parse_signer_fingerprint_from_str(content: &str) -> Option<String> {
    parse_signer_fingerprint(content)
}

fn parse_signer_fingerprint(content: &str) -> Option<String> {
    let first_line = content.lines().next()?;
    // Signature line format: "# ryeos:signed:<ts>:<hash>:<sig_b64>:<fingerprint>"
    let after_prefix = first_line.trim().strip_prefix("# ryeos:signed:")?;
    let parts: Vec<&str> = after_prefix.split(':').collect();
    // fingerprint is the last part (after the b64 sig which may contain = chars)
    // Format: timestamp:hash:base64sig:fingerprint
    if parts.len() >= 4 {
        Some(parts[parts.len() - 1].trim().to_string())
    } else {
        None
    }
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

        let yaml_content = r#"spec_version: 1
section: schedules
schedule_id: my-schedule
item_ref: "directive:test/hello"
schedule_type: interval
expression: "60"
"#;
        fs::write(sched_dir.join("my-schedule.yaml"), yaml_content).unwrap();

        let db = test_db();
        let live_ids = rebuild_specs_from_dir(&sched_dir, &db).unwrap();

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
        let live_ids = rebuild_specs_from_dir(&sched_dir, &db).unwrap();
        assert!(live_ids.is_empty());
    }

    #[test]
    fn rebuild_from_dir_skips_mismatched_id() {
        let dir = tempfile::tempdir().unwrap();
        let sched_dir = dir.path().join("schedules");
        fs::create_dir_all(&sched_dir).unwrap();

        let yaml = r#"spec_version: 1
section: schedules
schedule_id: wrong-id
item_ref: "directive:test/hello"
schedule_type: interval
expression: "60"
"#;
        fs::write(sched_dir.join("my-schedule.yaml"), yaml).unwrap();

        let db = test_db();
        let live_ids = rebuild_specs_from_dir(&sched_dir, &db).unwrap();
        assert!(live_ids.is_empty());
    }

    #[test]
    fn rebuild_from_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        let sched_dir = dir.path().join("schedules");
        fs::create_dir_all(&sched_dir).unwrap();

        let db = test_db();
        let live_ids = rebuild_specs_from_dir(&sched_dir, &db).unwrap();
        assert!(live_ids.is_empty());
    }

    #[test]
    fn rebuild_from_nonexistent_dir() {
        let db = test_db();
        let live_ids = rebuild_specs_from_dir(Path::new("/nonexistent/path"), &db).unwrap();
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

    // ── strip_signature ────────────────────────────────────────

    #[test]
    fn strip_signature_removes_header() {
        let content = "# ryeos:signed:2026-01-01T00:00:00Z:abc123:sig==:fp:abc\nspec_version: 1\nschedule_id: test";
        let body = strip_signature(content);
        assert!(!body.starts_with("# ryeos:signed:"));
        assert!(body.contains("spec_version: 1"));
    }

    #[test]
    fn strip_signature_no_header() {
        let content = "spec_version: 1\nschedule_id: test";
        let body = strip_signature(content);
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
}
