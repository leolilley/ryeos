//! Standalone audit NDJSON writer.
//!
//! Appends one NDJSON line per standalone service invocation to
//! `<system_space_dir>/.ai/state/audit/standalone.ndjson`.
//!
//! File is opened in append mode each call (no long-lived handle).
//! No automatic rotation in V5.2 — revisit if file grows past ~100MB
//! or standalone becomes automated.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Serialize;

/// A single audit record for a standalone service invocation.
#[derive(Debug, Serialize)]
pub struct StandaloneAuditRecord {
    /// ISO 8601 timestamp.
    pub ts: String,
    /// Always "standalone".
    pub mode: &'static str,
    /// Full service ref, e.g. "service:system/status".
    pub service_ref: String,
    /// Endpoint extracted from the service YAML.
    pub endpoint: String,
    /// "success" or "failure".
    pub status: &'static str,
    /// Error message (present only on failure).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    /// Unix UID of the calling process.
    pub uid: u32,
    /// PID of the calling process.
    pub pid: u32,
    /// Always "local-operator" in standalone mode.
    pub requested_by: &'static str,
    /// SHA-256 hex of the params JSON (for integrity, not replay).
    pub params_hash: String,
}

/// Write a standalone audit record as a single NDJSON line.
///
/// Creates the parent directory if it doesn't exist.
/// Opens the file in append mode, writes, and closes.
pub fn write_audit_record(
    audit_path: &Path,
    record: &StandaloneAuditRecord,
) -> Result<()> {
    if let Some(parent) = audit_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create audit dir {}", parent.display()))?;
    }

    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(audit_path)
        .with_context(|| format!("failed to open audit file {}", audit_path.display()))?;

    let line = serde_json::to_string(record)
        .context("failed to serialize audit record")?;
    writeln!(file, "{}", line)
        .with_context(|| format!("failed to write audit record to {}", audit_path.display()))?;

    Ok(())
}

/// Return the default standalone audit path for a given system space directory.
pub fn default_audit_path(system_space_dir: &Path) -> std::path::PathBuf {
    system_space_dir
        .join(".ai")
        .join("state")
        .join("audit")
        .join("standalone.ndjson")
}

/// Compute SHA-256 hex digest of the params JSON for audit integrity.
pub fn params_hash(params: &serde_json::Value) -> String {
    use sha2::Digest;
    let json_str = serde_json::to_string(params).unwrap_or_default();
    sha2::Sha256::digest(json_str.as_bytes())
        .iter()
        .map(|b| format!("{:02x}", b))
        .collect()
}

/// Get the current process UID.
#[cfg(unix)]
pub fn current_uid() -> u32 {
    unsafe { libc::geteuid() }
}

#[cfg(not(unix))]
pub fn current_uid() -> u32 {
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn writes_single_ndjson_line() {
        let tmpdir = TempDir::new().unwrap();
        let audit_path = tmpdir.path().join("audit.ndjson");

        let record = StandaloneAuditRecord {
            ts: "2026-01-01T00:00:00Z".into(),
            mode: "standalone",
            service_ref: "service:system/status".into(),
            endpoint: "system.status".into(),
            status: "success",
            error_message: None,
            uid: 1000,
            pid: 42,
            requested_by: "local-operator",
            params_hash: "deadbeef".into(),
        };

        write_audit_record(&audit_path, &record).unwrap();

        let content = std::fs::read_to_string(&audit_path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 1);
        let parsed: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(parsed["service_ref"], "service:system/status");
        assert_eq!(parsed["status"], "success");
    }

    #[test]
    fn appends_multiple_lines() {
        let tmpdir = TempDir::new().unwrap();
        let audit_path = tmpdir.path().join("audit.ndjson");

        for i in 0..3 {
            let record = StandaloneAuditRecord {
                ts: format!("2026-01-01T00:00:0{}Z", i),
                mode: "standalone",
                service_ref: format!("service:test/{}", i),
                endpoint: format!("test.{}", i),
                status: "success",
                error_message: None,
                uid: 0,
                pid: i,
                requested_by: "local-operator",
                params_hash: "abc".into(),
            };
            write_audit_record(&audit_path, &record).unwrap();
        }

        let content = std::fs::read_to_string(&audit_path).unwrap();
        assert_eq!(content.lines().count(), 3);
    }

    #[test]
    fn failure_record_includes_error_message() {
        let tmpdir = TempDir::new().unwrap();
        let audit_path = tmpdir.path().join("audit.ndjson");

        let record = StandaloneAuditRecord {
            ts: "2026-01-01T00:00:00Z".into(),
            mode: "standalone",
            service_ref: "service:test/fail".into(),
            endpoint: "test.fail".into(),
            status: "failure",
            error_message: Some("something went wrong".into()),
            uid: 0,
            pid: 1,
            requested_by: "local-operator",
            params_hash: "abc".into(),
        };

        write_audit_record(&audit_path, &record).unwrap();

        let content = std::fs::read_to_string(&audit_path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(content.trim()).unwrap();
        assert_eq!(parsed["status"], "failure");
        assert_eq!(parsed["error_message"], "something went wrong");
    }

    #[test]
    fn default_audit_path_is_under_ai_state() {
        use std::path::PathBuf;
        let path = default_audit_path(Path::new("/var/lib/ryeosd"));
        assert_eq!(
            path,
            PathBuf::from("/var/lib/ryeosd/.ai/state/audit/standalone.ndjson")
        );
    }

    #[test]
    fn params_hash_is_deterministic() {
        let params = serde_json::json!({"key": "value"});
        let h1 = params_hash(&params);
        let h2 = params_hash(&params);
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 64); // SHA-256 hex = 64 chars
    }

    #[test]
    fn uid_is_reasonable() {
        let uid = current_uid();
        // On Unix, should be a real UID; on non-Unix, returns 0
        #[cfg(unix)]
        assert!(uid < 65536);
    }
}
