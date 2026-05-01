use std::fs;
use std::path::Path;

use anyhow::Result;
use ryeos_engine::resolution::TrustClass;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::identity::NodeIdentity;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThreadMeta {
    pub thread_id: String,
    pub status: String,
    pub item_ref: String,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub limits: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub started_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outputs: Option<Value>,
    /// Daemon-computed executor trust posture (weakest of root +
    /// extends chain). Written here so the thread.json audit trail
    /// shows what trust class spawned the runtime.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub executor_trust_class: Option<TrustClass>,
}

pub fn write_thread_meta(
    project_root: &Path,
    thread_id: &str,
    meta: &ThreadMeta,
    identity: &NodeIdentity,
) -> Result<()> {
    let thread_dir = project_root
        .join(".ai")
        .join("state")
        .join("threads")
        .join(thread_id);
    fs::create_dir_all(&thread_dir)?;

    let json = serde_json::to_string_pretty(meta)?;
    let signed = lillux::signature::sign_content(&json, identity.signing_key(), "#", None);

    let path = thread_dir.join("thread.json");
    let tmp = thread_dir.join("thread.json.tmp");
    fs::write(&tmp, signed)?;
    fs::rename(&tmp, &path)?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thread_meta_serialization() {
        let meta = ThreadMeta {
            thread_id: "T-test".to_string(),
            status: "running".to_string(),
            item_ref: "directive:my/agent".to_string(),
            capabilities: vec!["rye.execute.tool.*".to_string()],
            limits: serde_json::json!({"turns": 25}),
            model: Some("anthropic/claude".to_string()),
            started_at: "2026-04-19T00:00:00Z".to_string(),
            completed_at: None,
            cost: None,
            outputs: None,
            executor_trust_class: Some(TrustClass::TrustedSystem),
        };

        let json = serde_json::to_string(&meta).unwrap();
        // Enum serializes to lowercase snake_case (no `format!("{:?}")` hack).
        assert!(
            json.contains("\"executor_trust_class\":\"trusted_system\""),
            "expected snake_case enum serialization, got: {json}"
        );
        let parsed: ThreadMeta = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.thread_id, "T-test");
        assert_eq!(parsed.status, "running");
        assert_eq!(parsed.executor_trust_class, Some(TrustClass::TrustedSystem));
    }
}
