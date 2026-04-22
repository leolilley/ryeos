//! Shared utilities for rye-core-tools
//! 
//! - Environment parsing
//! - Output formatting
//! - Common traits

use anyhow::{Context, Result};

/// Parse RYE_STATE_ROOT environment variable.
///
/// Fails explicitly if the environment variable is not set.
/// (No fallback to current directory — that would silently operate on wrong state.)
pub fn get_state_root() -> Result<std::path::PathBuf> {
    let root = std::env::var("RYE_STATE_ROOT")
        .context("RYE_STATE_ROOT environment variable not set")?;
    Ok(std::path::PathBuf::from(root))
}

/// Format JSON output
pub fn format_json<T: serde::Serialize>(value: &T) -> Result<String> {
    Ok(serde_json::to_string_pretty(value)?)
}

/// Colored status output
pub fn output_success(msg: &str) {
    println!("✓ {}", msg);
}

pub fn output_error(msg: &str) {
    eprintln!("✗ {}", msg);
}

pub fn output_info(msg: &str) {
    println!("ℹ {}", msg);
}

/// Status report structure
#[derive(Debug, serde::Serialize)]
pub struct StatusReport {
    pub rye_state: String,
    pub cas_root: String,
    pub chains_count: usize,
    pub last_updated: String,
    pub projection_status: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_status_report_serializes() {
        let report = StatusReport {
            rye_state: "/tmp/test".to_string(),
            cas_root: "/tmp/test/.state/objects".to_string(),
            chains_count: 5,
            last_updated: "2026-04-22T00:00:00Z".to_string(),
            projection_status: "ok".to_string(),
        };
        let json = format_json(&report).unwrap();
        assert!(json.contains("\"rye_state\""));
    }
}
